//! 共享代际 / 存活槽表（白送优化）：sim 行存与 render sidecar 的**行 / 存活内核共同底座**。
//!
//! ## 与 [`crate::column`] 是一对孪生底座
//! 一个 SoA 存储分两层正交关注点，各拆成可独立替换的半边：
//! - [`Column`][crate::column::Column]：类型化无装箱**数据**列，按行号寻址、**与生命周期无关**。
//! - `GenSlots`（本模块）：每槽的**代际号 + 存活位**，按槽号寻址、**与身份模型无关**。
//!
//! 两者都只认 `usize` 槽号、不认实例身份——上层的**行 / 存活内核**（决定「一个槽
//! 代表谁」）坐在它们之上，各自一份、可独立替换：
//! - sim 侧（[`crate::runtime::store`]）：代际 `id_slot` 间接（id→row）+ [`RowPolicy`]
//!   压缩 / 留洞 + `row_id` 反查。槽 = **行号**，代际供迭代时反向构造 `InstanceId`。
//! - render 侧（[`crate::render::store`]）：`槽 = sim id` 直址，被动跟随 sim 生灭。
//!   代际兼作正向 ABA 校验（[`GenSlots::matches`]）。
//!
//! 这两套身份机天差地别（id 复用 / 行压缩 vs 直址跟随），**故意不抽成 trait**——
//! 强行统一只会把正交的演化轴焊死。GenSlots 只共享两者真正同构的那层：代际号与
//! 存活位的 SoA 簿记 + ABA 校验 + 稠密存活扫描。语义在两侧一致：
//! `generation(slot)` 恒为「该槽当前住户的代际号」、`is_live(slot)` 恒为「该槽被占用」。
//!
//! [`RowPolicy`]: crate::runtime::RowPolicy

/// 空槽的代际占位（render 被动扩容出、尚无住户的槽）。存活位为 false 时其代际值不
/// 参与任何判定（[`GenSlots::matches`] 先验存活），此哨兵仅为可读性与确定性。
const VACANT: u64 = u64::MAX;

/// 代际号 + 存活位的 SoA 槽表。按槽号（`usize`）寻址、与身份模型无关——上层的行 /
/// 存活内核（sim 的 `id_slot`+[`RowPolicy`][crate::runtime::RowPolicy] 或 render 的
/// id 直址）自定义「槽代表谁」。代际推进（ABA 防护的 +1）归上层身份机所有，本结构
/// 只忠实记录「当前住户的代际」。
#[derive(Debug, Clone, Default)]
pub(crate) struct GenSlots {
    /// 每槽当前住户的代际号（反向构造 `InstanceId` / 正向 ABA 校验）。空槽为 [`VACANT`]。
    /// 命名避开 edition 2024 保留字 `gen`。
    generations: Vec<u64>,
    /// 每槽是否被占用（sim Stable 的洞 / render 未出生槽为 false）。
    live: Vec<bool>,
}

impl GenSlots {
    pub(crate) fn new() -> GenSlots {
        GenSlots {
            generations: vec![],
            live: vec![],
        }
    }

    /// 槽总数（含洞）。sim 行追加 / render 列扩容据此对齐平行数组长度。
    #[allow(clippy::len_without_is_empty)]
    pub(crate) fn len(&self) -> usize {
        self.live.len()
    }

    /// 该槽当前住户的代际号。仅在调用方已确认槽存在 / 占用时取（迭代反向构造用）。
    #[inline]
    pub(crate) fn generation(&self, slot: usize) -> u64 {
        self.generations[slot]
    }

    /// 该槽是否被占用。
    #[inline]
    pub(crate) fn is_live(&self, slot: usize) -> bool {
        self.live[slot]
    }

    /// ABA 校验（正向解析用）：槽存在、被占用、且住户代际与查询一致。
    /// 越界 / 空置 / 代际不符（旧 ref 指向已复用槽）→ false。
    #[inline]
    pub(crate) fn matches(&self, slot: usize, generation: u64) -> bool {
        slot < self.live.len() && self.live[slot] && self.generations[slot] == generation
    }

    /// 追加一个占用槽（住户代际 = `generation`），返回其槽号。sim 行追加分配走这里。
    pub(crate) fn push_live(&mut self, generation: u64) -> usize {
        self.generations.push(generation);
        self.live.push(true);
        self.live.len() - 1
    }

    /// 占用一个已存在的槽（置住户代际 = `generation`、置存活）。sim Stable 复用洞 /
    /// render 出生走这里（调用方须先确保槽存在——sim 复用 `free_rows`、render 经
    /// [`GenSlots::grow_to`]）。
    #[inline]
    pub(crate) fn activate(&mut self, slot: usize, generation: u64) {
        self.generations[slot] = generation;
        self.live[slot] = true;
    }

    /// 释放一个槽（留位、清存活）。sim Stable 死亡留洞 / render 死亡走这里。代际号
    /// **不在此 +1**——ABA 推进归上层身份机（sim 在 `id_slot`、render 在出生重置）。
    #[inline]
    pub(crate) fn kill(&mut self, slot: usize) {
        self.live[slot] = false;
    }

    /// 末槽搬入 `slot` 并截短（sim Compact 死亡的稠密重映射）。调用方负责同步搬移
    /// 其平行数组（`row_id` / 各 [`Column`][crate::column::Column]）并修正被搬末槽
    /// 住户的 id→行间接。
    pub(crate) fn swap_remove(&mut self, slot: usize) {
        self.generations.swap_remove(slot);
        self.live.swap_remove(slot);
    }

    /// 增长到至少 `n` 槽，新增槽空置（[`VACANT`] / 非存活）。render sidecar 被动跟随
    /// sim 分配新 id 时扩容用；已够长则无操作（行只增不缩，死亡留洞复用）。
    pub(crate) fn grow_to(&mut self, n: usize) {
        if n > self.live.len() {
            self.generations.resize(n, VACANT);
            self.live.resize(n, false);
        }
    }

    /// 稠密扫描全部占用槽，回调 `(槽号, 住户代际)`。识别身份（构造 `InstanceId`）归
    /// 调用方——本结构身份无关。
    pub(crate) fn for_each_live(&self, mut f: impl FnMut(usize, u64)) {
        for slot in 0..self.live.len() {
            if self.live[slot] {
                f(slot, self.generations[slot]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_live_returns_slot_and_marks_present() {
        let mut s = GenSlots::new();
        assert_eq!(s.push_live(0), 0);
        assert_eq!(s.push_live(0), 1);
        assert_eq!(s.len(), 2);
        assert!(s.is_live(0) && s.is_live(1));
        assert!(s.matches(0, 0) && s.matches(1, 0));
    }

    #[test]
    fn matches_enforces_generation_and_bounds() {
        let mut s = GenSlots::new();
        s.push_live(7);
        assert!(s.matches(0, 7));
        assert!(!s.matches(0, 6), "代际不符（旧 ref）→ 不匹配");
        assert!(!s.matches(1, 7), "越界 → 不匹配");
    }

    #[test]
    fn kill_clears_presence_but_keeps_slot() {
        let mut s = GenSlots::new();
        s.push_live(3);
        s.kill(0);
        assert_eq!(s.len(), 1, "留位");
        assert!(!s.is_live(0));
        assert!(!s.matches(0, 3), "已释放槽不匹配任何代际");
    }

    #[test]
    fn activate_reoccupies_hole_with_new_generation() {
        let mut s = GenSlots::new();
        s.push_live(0);
        s.kill(0);
        s.activate(0, 1); // 复用洞，住户代际推进由上层传入
        assert!(s.matches(0, 1));
        assert!(!s.matches(0, 0), "旧代际不再可达（ABA 防护）");
    }

    #[test]
    fn grow_to_adds_vacant_slots_idempotently() {
        let mut s = GenSlots::new();
        s.grow_to(3);
        assert_eq!(s.len(), 3);
        assert!(!s.is_live(0) && !s.is_live(2), "新槽空置");
        s.grow_to(2); // 已够长 → 无操作（不截短）
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn swap_remove_compacts_last_into_hole() {
        let mut s = GenSlots::new();
        s.push_live(10); // slot 0
        s.push_live(11); // slot 1
        s.push_live(12); // slot 2
        s.swap_remove(0); // 末槽（gen 12）搬入 0
        assert_eq!(s.len(), 2);
        assert_eq!(s.generation(0), 12);
        assert!(s.matches(0, 12) && s.matches(1, 11));
    }

    #[test]
    fn for_each_live_visits_only_present_slots() {
        let mut s = GenSlots::new();
        s.push_live(0); // 0
        s.push_live(0); // 1
        s.push_live(0); // 2
        s.kill(1);
        let mut seen = vec![];
        s.for_each_live(|slot, g| seen.push((slot, g)));
        assert_eq!(seen, vec![(0, 0), (2, 0)], "跳过被释放的槽 1");
    }
}
