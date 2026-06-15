//! sidecar 渲染存储：render 私有字段 + tracked sim 字段的 (prev, cur) 镜像。
//!
//! 你定的布局：render **共享读** sim 存储（经 [`super::handoff::SimFrame`] 冻结
//! 快照），但 render 自算的字段必然另有归宿——就是这里。两套命名空间互不冲突：
//! - sim 字段：[`FieldId`]，render 只读（镜像在本存储的 prev/cur 列）；
//! - render 字段：[`RFieldId`]，render 独占写（D1 render 侧）。
//!
//! 行按 sim 实例 id 直接索引（与 sim 存储同构）；代际号防 ABA：sim 回收复用某 id
//! 后，render 行经代际比较识别为新住户并重置。生命周期被动跟随 sim 的生灭增量
//! ——render 从不创建/销毁**共享实体**（你的约束），只维护自己这面镜子。
//!
//! ## 两层共享底座：与 sim 同款的类型化列内核 + 代际 / 存活槽表（seam）
//! render 每帧的稠密扫（插值、画序、剔除）是顺序列访问——与 sim 存储**共用**两层底座：
//! - **数据**：`Column` 类型化去装箱列（白送优化 A3）。三组列都无装箱：render 私有
//!   字段列、每个 track 的 prev / cur 镜像列。最热的每帧 transform 插值输出因此落进
//!   `Vec3` / `Quat` / `Float` 无装箱列（而非旧版 `Vec<Value>` 逐格宽枚举 + 判别位）
//!   ——`track` 输出字段按插值种类定型的收益落点（见 [`super::RenderRuntime::track`] /
//!   [`super::Interp::out_default`]）。
//! - **代际 / 存活**：每行的代际号 + 存活位走共享 `GenSlots`——render 出生 / 死亡 /
//!   ABA 校验 / 稠密存活扫描全经它。
//!
//! 两层底座共享、**行 / 存活内核（身份机）各管各的**：sim 侧是代际 `id_slot` 间接 +
//! `RowPolicy`；render 侧（本模块）是 `行 = sim id` 直址，被动跟随 sim 生灭。两者只透过
//! `Column` / `GenSlots` 的槽寻址 API 触碰底座，故能独立替换。

use crate::column::Column;
use crate::entity::{EntityTypeId, FieldId, InstanceId};
use crate::genslots::GenSlots;
use crate::value::Value;

/// render 字段标识（render 命名空间，每类型独立编号）。与 sim 的 [`FieldId`] 不冲突。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RFieldId(pub u32);

/// 一个被 track 的 sim 字段的镜像列：(prev, cur) 双缓冲（类型化无装箱，按 sim 字段
/// 默认值定型）。
/// 「本区间是否在动」由 [`super::RenderRuntime`] 的活动集唯一裁定，不在此重复记账
/// （避免两套成员资格在多帧 drain 下分歧）。
struct TrackCol {
    /// 镜像的 sim 字段（调试元数据；定位经 [`super::Track::slot`]）。
    #[allow(dead_code)]
    sim_field: FieldId,
    /// 该 sim 字段的 schema 默认值，出生时 snap 用（render 只见增量，未写出的字段
    /// 须从此取值，否则 out 永远停在 Null）。同时是 prev/cur 列的定型种子。
    default: Value,
    prev: Column,
    cur: Column,
}

struct RenderTypeStore {
    render_defaults: Vec<Value>,
    /// SoA：render_cols[rfield] 是类型化无装箱列，行号 = sim id。
    render_cols: Vec<Column>,
    tracks: Vec<TrackCol>,
    /// 每行（= sim id）的代际号 + 存活位（共享行 / 存活底座）。代际兼作正向 ABA 校验：
    /// sim 复用某 id 后，旧代际 ref 经 [`GenSlots::matches`] 识别为已换住户。
    slots: GenSlots,
}

impl RenderTypeStore {
    /// 行容量增长到至少容纳 `id`（新建行以默认值填充）。sim 分配新 id 时被动跟随。
    fn ensure_row(&mut self, id: usize) {
        if id < self.slots.len() {
            return;
        }
        let n = id + 1;
        for (fi, col) in self.render_cols.iter_mut().enumerate() {
            col.resize(n, &self.render_defaults[fi]);
        }
        for tc in &mut self.tracks {
            tc.prev.resize(n, &tc.default);
            tc.cur.resize(n, &tc.default);
        }
        self.slots.grow_to(n);
    }
}

/// render sidecar 存储：每 sim 类型一个镜像 [`RenderTypeStore`]。
pub(crate) struct RenderStore {
    types: Vec<RenderTypeStore>,
}

impl RenderStore {
    /// 从 sim 类型数构造：每个 sim 类型预留一个空镜像（render 字段/track 后续追加）。
    pub(crate) fn new(sim_type_count: usize) -> Self {
        let mut types = Vec::with_capacity(sim_type_count);
        for _ in 0..sim_type_count {
            types.push(RenderTypeStore {
                render_defaults: vec![],
                render_cols: vec![],
                tracks: vec![],
                slots: GenSlots::new(),
            });
        }
        RenderStore { types }
    }

    /// 在某类型上注册一个 render 字段，返回其 [`RFieldId`]。列按 `default` 定型
    /// （类型化无装箱——`track` 据插值种类传入 Vec3/Quat/Float 默认值即得去装箱列）。
    pub(crate) fn add_render_field(&mut self, ty: EntityTypeId, default: Value) -> RFieldId {
        let t = &mut self.types[ty.0 as usize];
        let id = t.render_cols.len() as u32;
        let rows = t.slots.len();
        t.render_cols.push(Column::with_default(&default, rows));
        t.render_defaults.push(default);
        RFieldId(id)
    }

    /// 在某类型上注册一个 tracked sim 字段镜像，返回其局部 track 槽位下标。
    /// `default` 是该 sim 字段的 schema 默认值（出生 snap 用，并为 prev/cur 列定型）。
    pub(crate) fn add_track(
        &mut self,
        ty: EntityTypeId,
        sim_field: FieldId,
        default: Value,
    ) -> usize {
        let t = &mut self.types[ty.0 as usize];
        let rows = t.slots.len();
        let slot = t.tracks.len();
        t.tracks.push(TrackCol {
            sim_field,
            prev: Column::with_default(&default, rows),
            cur: Column::with_default(&default, rows),
            default,
        });
        slot
    }

    pub(crate) fn has_type(&self, ty: EntityTypeId) -> bool {
        self.types.get(ty.0 as usize).is_some()
    }

    pub(crate) fn has_render_field(&self, ty: EntityTypeId, f: RFieldId) -> bool {
        self.types
            .get(ty.0 as usize)
            .is_some_and(|t| (f.0 as usize) < t.render_cols.len())
    }

    #[inline]
    fn row_of(&self, inst: InstanceId) -> Option<usize> {
        let t = self.types.get(inst.ty.0 as usize)?;
        let id = inst.id as usize;
        t.slots.matches(id, inst.generation).then_some(id)
    }

    /// 出生：sim 写出 `_alive = true` 时由 render 摄入。重置 render 字段为默认值，
    /// 并把每个 tracked 镜像 snap 到该字段的 sim 默认值（prev = cur = default）。
    /// 若同帧 spawn 的 init 携带了该字段的值，随后的 `apply_delta(just_born)` 会以
    /// 初值覆盖此 snap；未携带的字段则正确停在 schema 默认值（而非 Null）。
    pub(crate) fn birth(&mut self, inst: InstanceId) {
        let t = &mut self.types[inst.ty.0 as usize];
        let id = inst.id as usize;
        t.ensure_row(id);
        t.slots.activate(id, inst.generation);
        for (fi, col) in t.render_cols.iter_mut().enumerate() {
            col.set(id, t.render_defaults[fi].clone());
        }
        for tc in &mut t.tracks {
            tc.prev.set(id, tc.default.clone());
            tc.cur.set(id, tc.default.clone());
        }
    }

    /// 死亡：sim 写出 `_alive = false` 结算后由 render 摄入。行留洞待复用。
    pub(crate) fn death(&mut self, inst: InstanceId) {
        if let Some(id) = self.row_of(inst) {
            let t = &mut self.types[inst.ty.0 as usize];
            t.slots.kill(id);
            for (fi, col) in t.render_cols.iter_mut().enumerate() {
                col.set(id, t.render_defaults[fi].clone());
            }
            for tc in &mut t.tracks {
                tc.prev.set(id, tc.default.clone());
                tc.cur.set(id, tc.default.clone());
            }
        }
    }

    /// 摄入一条 tracked 字段增量：设 prev = old、cur = new。
    /// 刚出生的实例其初值增量应 snap（prev = new），由 `just_born` 指明。
    pub(crate) fn apply_delta(
        &mut self,
        inst: InstanceId,
        slot: usize,
        old: Value,
        new: Value,
        just_born: bool,
    ) -> bool {
        let Some(id) = self.row_of(inst) else {
            return false;
        };
        let tc = &mut self.types[inst.ty.0 as usize].tracks[slot];
        tc.prev.set(id, if just_born { new.clone() } else { old });
        tc.cur.set(id, new);
        true
    }

    /// 取某 track 槽的 (prev, cur)。
    pub(crate) fn track_pair(&self, inst: InstanceId, slot: usize) -> Option<(Value, Value)> {
        let id = self.row_of(inst)?;
        let tc = &self.types[inst.ty.0 as usize].tracks[slot];
        Some((tc.prev.get(id), tc.cur.get(id)))
    }

    pub(crate) fn read_render(&self, inst: InstanceId, f: RFieldId) -> Value {
        match self.row_of(inst) {
            Some(id) => self.types[inst.ty.0 as usize]
                .render_cols
                .get(f.0 as usize)
                .map_or(Value::Null, |c| c.get(id)),
            None => Value::Null,
        }
    }

    pub(crate) fn write_render(&mut self, inst: InstanceId, f: RFieldId, v: Value) {
        if let Some(id) = self.row_of(inst) {
            if let Some(col) = self.types[inst.ty.0 as usize]
                .render_cols
                .get_mut(f.0 as usize)
            {
                col.set(id, v);
            }
        }
    }

    /// 该实例在 render 侧是否在场（行存活且代际匹配）。语义 ≠ sim `_alive`：死亡淡出
    /// 窗口内 render 行仍在场（尸体未回收），故 render「在场」= sim 存活 ⊔ 淡出尸体。
    pub(crate) fn is_present(&self, inst: InstanceId) -> bool {
        self.row_of(inst).is_some()
    }

    /// 稠密遍历某类型的存活 render 行（连续更新 / 剔除扫的入口）。
    pub(crate) fn for_each_live(&self, ty: EntityTypeId, mut f: impl FnMut(InstanceId)) {
        let t = &self.types[ty.0 as usize];
        t.slots.for_each_live(|id, generation| {
            f(InstanceId {
                ty,
                id: id as u32,
                generation,
            });
        });
    }
}
