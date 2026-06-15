//! 空间 / 范围广相位索引（C 层「有代价」优化：碰撞、AoI、范围查询）。
//!
//! 按架构 §6.1，空间查询**不进谓词层**，而是物化为**索引实体**：建一个 singleton
//! 索引 entity，batch 订阅原始 position 写（`type(Unit, position) batch
//! deliver(writer_id, old, new)`，见 docs §7 示例 3），在其 calculation 里增量维护
//! 本结构，把查询结果写进自己的字段供下游订阅。这样谓词词汇保持封闭，同时把
//! 最优增量维护路径作为**工具**交给用户代码——不是新引擎原语（calc 本就是图灵
//! 完备代码），不破「四层封闭」。
//!
//! ## 为什么给接口而非做满（架构哲学：有代价 → 接口 + 上游集成）
//! 没有普适最优的空间结构：均匀网格 / 层次网格 / BVH / sweep-and-prune 取决于
//! 密度、分布、查询类型，须按实际硬件与游戏负载权衡——架构不替开发者拍板。这里
//! 给一个**均匀网格**实现（移动物 + 近邻交互的常见甜点），cell 尺寸由开发者定；
//! 需要别的结构时照同一查询接口换实现即可。
//!
//! ## 三类查询
//! - [`SpatialGrid::query_aabb`]：矩形范围查询。
//! - [`SpatialGrid::query_radius`]：半径查询（AoI / 范围查询）。
//! - [`SpatialGrid::candidate_pairs`]：广相位候选对（碰撞，narrow-phase 由调用方做）。
//!
//! 增量更新 O(1) 摊销（仅换格时动桶），恰由 batch position 增量喂入；移除由
//! 死亡订阅（`type(Unit, _alive) where became(false)`）或 became(null) 驱动。
//!
//! 上游集成（端到端走法）见 `tests/spatial_index.rs`：索引实体 batch 喂网格、
//! 把候选对 / AoI 结果写进自己字段、下游订阅这些普通字段。
//!
//! ## 快照注意
//! 网格是 position 的**派生**状态，按 §6.1 属索引实体私有增量态，不在 sim store
//! 内，故 [`crate::Snapshot`] 不含它——rollback 后由当前 position 重建（或开发者
//! 自行随快照另存）。cell 尺寸应 ≥ 最大交互半径，否则 [`candidate_pairs`] 的
//! 8 邻域扫描会漏掉跨多格的对。
//!
//! [`candidate_pairs`]: SpatialGrid::candidate_pairs

use std::collections::HashMap;

use crate::entity::InstanceId;

/// 一个网格住户：所在格 + 精确坐标（窄相位过滤用）。
#[derive(Clone, Copy)]
struct Entry {
    cell: (i32, i32),
    x: f64,
    y: f64,
}

/// 均匀网格广相位索引。2D、`f64` 坐标、按 [`InstanceId`] 键入。
///
/// 在索引实体的 calculation 里持有（典型经 `Arc<Mutex<SpatialGrid>>` 由 calc 闭包
/// 捕获——单索引实例每帧只运行一次，无并行竞争），用 batch position 增量喂入。
pub struct SpatialGrid {
    /// 1 / cell_size，换格运算用乘代替除。
    inv_cell: f64,
    cell_size: f64,
    /// 格 → 该格住户列表。
    cells: HashMap<(i32, i32), Vec<InstanceId>>,
    /// 住户 → 其格与坐标（增量换格、窄相位过滤）。
    entries: HashMap<InstanceId, Entry>,
}

/// 升序确定键：让查询结果有确定序（lockstep / 回放友好，配合 C4 Canonical）。
#[inline]
fn key(i: &InstanceId) -> (u32, u32, u64) {
    (i.ty.0, i.id, i.generation)
}

impl SpatialGrid {
    /// 新建网格。`cell_size` 须 > 0；应 ≥ 最大交互半径 / 物体直径（见模块文档）。
    pub fn new(cell_size: f64) -> Self {
        assert!(
            cell_size.is_finite() && cell_size > 0.0,
            "cell_size 必须为有限正数"
        );
        SpatialGrid {
            inv_cell: 1.0 / cell_size,
            cell_size,
            cells: HashMap::new(),
            entries: HashMap::new(),
        }
    }

    pub fn cell_size(&self) -> f64 {
        self.cell_size
    }

    /// 当前住户数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 某住户当前的精确坐标（不在网格则 `None`）。供消费方在广相位命中后算精确距离
    /// （如 render 剔除的 LOD 距离），复用网格已存的位、免回原存储重读；与查询的成员
    /// 判定同源（都基于这份 `entries`）。
    #[inline]
    pub fn position(&self, id: InstanceId) -> Option<(f64, f64)> {
        self.entries.get(&id).map(|e| (e.x, e.y))
    }

    #[inline]
    fn cell_of_checked(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        fn axis(v: f64, inv_cell: f64) -> Option<i32> {
            if !v.is_finite() {
                return None;
            }
            let c = (v * inv_cell).floor();
            (c >= i32::MIN as f64 && c <= i32::MAX as f64).then_some(c as i32)
        }
        Some((axis(x, self.inv_cell)?, axis(y, self.inv_cell)?))
    }

    #[inline]
    fn cell_of_clamped(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        fn axis(v: f64, inv_cell: f64) -> Option<i32> {
            if !v.is_finite() {
                return None;
            }
            let c = (v * inv_cell).floor();
            if c < i32::MIN as f64 {
                Some(i32::MIN)
            } else if c > i32::MAX as f64 {
                Some(i32::MAX)
            } else {
                Some(c as i32)
            }
        }
        Some((axis(x, self.inv_cell)?, axis(y, self.inv_cell)?))
    }

    /// 插入或移动一个住户到坐标 (x, y)。换格才动桶（O(1) 摊销）；恰由
    /// `batch deliver(writer_id, old, new)` 的 new 位置增量喂入。
    pub fn update(&mut self, id: InstanceId, x: f64, y: f64) {
        let nc = self
            .cell_of_checked(x, y)
            .expect("SpatialGrid 坐标必须为有限值且落在 i32 cell 范围内");
        match self.entries.get(&id).copied() {
            Some(e) if e.cell == nc => {} // 同格，只更新坐标
            Some(e) => {
                self.detach(id, e.cell);
                self.cells.entry(nc).or_default().push(id);
            }
            None => {
                self.cells.entry(nc).or_default().push(id);
            }
        }
        self.entries.insert(id, Entry { cell: nc, x, y });
    }

    /// 移除一个住户（死亡 / 离场）。不存在则无操作。
    pub fn remove(&mut self, id: InstanceId) {
        if let Some(e) = self.entries.remove(&id) {
            self.detach(id, e.cell);
        }
    }

    /// 移除同一逻辑槽位（同 type/id、任意 generation）的所有住户。render culling 在
    /// same-frame destroy+spawn 复用 id 时用它清理旧代际，避免 ABA 幽灵。
    pub(crate) fn remove_slot(&mut self, id: InstanceId) {
        let stale: Vec<InstanceId> = self
            .entries
            .keys()
            .copied()
            .filter(|e| e.ty == id.ty && e.id == id.id)
            .collect();
        for stale in stale {
            self.remove(stale);
        }
    }

    fn detach(&mut self, id: InstanceId, cell: (i32, i32)) {
        if let Some(v) = self.cells.get_mut(&cell) {
            if let Some(p) = v.iter().position(|&x| x == id) {
                v.swap_remove(p);
            }
            if v.is_empty() {
                self.cells.remove(&cell);
            }
        }
    }

    /// 矩形范围查询：返回精确坐标落在闭区间 [min, max]² 内的全部住户（确定序）。
    pub fn query_aabb(&self, min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<InstanceId> {
        if min_x > max_x
            || min_y > max_y
            || !min_x.is_finite()
            || !min_y.is_finite()
            || !max_x.is_finite()
            || !max_y.is_finite()
        {
            return vec![];
        }
        let Some((cx0, cy0)) = self.cell_of_clamped(min_x, min_y) else {
            return vec![];
        };
        let Some((cx1, cy1)) = self.cell_of_clamped(max_x, max_y) else {
            return vec![];
        };
        if self.range_too_wide(cx0, cy0, cx1, cy1) {
            return self.query_aabb_by_entries(min_x, min_y, max_x, max_y);
        }
        let mut out = vec![];
        for cx in cx0..=cx1 {
            for cy in cy0..=cy1 {
                if let Some(v) = self.cells.get(&(cx, cy)) {
                    for &id in v {
                        let e = &self.entries[&id];
                        if e.x >= min_x && e.x <= max_x && e.y >= min_y && e.y <= max_y {
                            out.push(id);
                        }
                    }
                }
            }
        }
        out.sort_by_key(key);
        out
    }

    /// 半径查询（AoI / 范围查询）：返回到 (cx, cy) 欧氏距离 ≤ r 的全部住户（确定序）。
    pub fn query_radius(&self, cx: f64, cy: f64, r: f64) -> Vec<InstanceId> {
        if !cx.is_finite() || !cy.is_finite() || !r.is_finite() || r < 0.0 {
            return vec![];
        }
        let r2 = r * r;
        let Some((gx0, gy0)) = self.cell_of_clamped(cx - r, cy - r) else {
            return vec![];
        };
        let Some((gx1, gy1)) = self.cell_of_clamped(cx + r, cy + r) else {
            return vec![];
        };
        if self.range_too_wide(gx0, gy0, gx1, gy1) {
            return self.query_radius_by_entries(cx, cy, r2);
        }
        let mut out = vec![];
        for gx in gx0..=gx1 {
            for gy in gy0..=gy1 {
                if let Some(v) = self.cells.get(&(gx, gy)) {
                    for &id in v {
                        let e = &self.entries[&id];
                        let (dx, dy) = (e.x - cx, e.y - cy);
                        if dx * dx + dy * dy <= r2 {
                            out.push(id);
                        }
                    }
                }
            }
        }
        out.sort_by_key(key);
        out
    }

    fn range_too_wide(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> bool {
        let width = (x1 as i64 - x0 as i64 + 1) as u128;
        let height = (y1 as i64 - y0 as i64 + 1) as u128;
        let scanned_cells = width.saturating_mul(height);
        let exact_scan_budget = ((self.entries.len() as u128).saturating_mul(4)).max(64);
        scanned_cells > exact_scan_budget
    }

    fn query_aabb_by_entries(
        &self,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    ) -> Vec<InstanceId> {
        let mut out = vec![];
        for (&id, e) in &self.entries {
            if e.x >= min_x && e.x <= max_x && e.y >= min_y && e.y <= max_y {
                out.push(id);
            }
        }
        out.sort_by_key(key);
        out
    }

    fn query_radius_by_entries(&self, cx: f64, cy: f64, r2: f64) -> Vec<InstanceId> {
        let mut out = vec![];
        for (&id, e) in &self.entries {
            let (dx, dy) = (e.x - cx, e.y - cy);
            if dx * dx + dy * dy <= r2 {
                out.push(id);
            }
        }
        out.sort_by_key(key);
        out
    }

    /// 广相位碰撞候选对：返回处于同格或相邻格的无序住户对（每对恰一次，确定序）。
    /// 这是**广相位**——只保证「足够近、值得做窄相位」；精确判定（距离 / 形状）
    /// 由调用方完成。前提：cell_size ≥ 最大交互直径（否则漏跨多格的对）。
    pub fn candidate_pairs(&self) -> Vec<(InstanceId, InstanceId)> {
        // 前向半邻域：每对反极偏移只取其一，跨格对不重复计数。
        const FWD: [(i32, i32); 4] = [(1, 0), (1, 1), (0, 1), (-1, 1)];
        let mut out = vec![];
        for (&(cx, cy), members) in &self.cells {
            // 同格内的全部无序对
            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    out.push(ordered(members[i], members[j]));
                }
            }
            // 与前向邻格的跨格对
            for (dx, dy) in FWD {
                let Some(nx) = cx.checked_add(dx) else {
                    continue;
                };
                let Some(ny) = cy.checked_add(dy) else {
                    continue;
                };
                if let Some(neigh) = self.cells.get(&(nx, ny)) {
                    for &a in members {
                        for &b in neigh {
                            out.push(ordered(a, b));
                        }
                    }
                }
            }
        }
        out.sort_by_key(|p| (key(&p.0), key(&p.1)));
        out
    }
}

/// 把一对实例规范化为 (小, 大)（按确定键），使无序对有稳定表示。
#[inline]
fn ordered(a: InstanceId, b: InstanceId) -> (InstanceId, InstanceId) {
    if key(&a) <= key(&b) { (a, b) } else { (b, a) }
}
