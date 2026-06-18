//! render runtime：第二个 runtime 实例，动态帧率，消费者角色。
//!
//! 设计定理（见会话分析）：**render runtime = simulation runtime − 生命周期
//! + 动态时钟 + sim 写日志摄入口 + 插值原语**。四层封闭不破——render 逻辑同样由
//! predicate + calculation 构建，只是：
//!
//! 1. **触发源有两条**（与 sim 的「唯一触发源」对偶）：
//!    - render clock tick → 连续更新（[`RenderRuntime::continuous`]），ECS 稠密扫，
//!      render 的主热路径（插值天生每帧）；
//!    - 摄入的 sim 写日志 → 事件反应（[`RenderRuntime::reaction`]），复用谓词代数。
//! 2. **共享生命周期 sim 独占**：render 无 spawn/destroy 共享实体的入口（你的约束）；
//!    render 的 sidecar 行被动跟随 sim 的生灭增量；若配置死亡淡出，render 只延迟
//!    回收自己的 sidecar 行，不改变 sim 生死事实。
//! 3. **字段级 D1 单向依赖**：render 只写 render 字段（[`store::RFieldId`]）、只读
//!    sim 字段（经 tracked 镜像）；sim 永不读 render——并发解耦的结构强制（A7）。
//!
//! ## 三类注册
//! - [`RenderRuntime::track`]：声明镜像某 sim 字段并按 [`Interp`] 维护插值输出
//!   （`fold` 的 render 对偶，runtime 自动每帧维护，calc 不手算）。
//! - [`RenderRuntime::reaction`]：sim 写谓词 → render 字段写（死亡淡出、命中火花、
//!   状态切换起动画）。v1 订阅者 = writer 本身。
//! - [`RenderRuntime::continuous`]：render clock 每帧对每个在场实例运行（相机阻尼、
//!   派生 transform）。
//!
//! ## 剔除 / LOD（Cr3，非第四注册概念）
//! [`RenderRuntime::enable_culling`] + [`RenderRuntime::cull_type`] 把 render 自维护的
//! [`SpatialGrid`] 接成 **§6.1「物化为索引实体」的 render 对偶**：相机每 render 帧查询
//! 网格得**可见集**，`continuous` 与 `submit` 收窄到视域内（离屏不重算 / 不提交）。LOD
//! 距离作为派生 render 字段暴露，分档由开发者持有（见 [`visible`]）。行为仍是三类注册，
//! 剔除只是个索引 + 查询，未启用时与今日逐字相同。
//!
//! ## 成本（render 侧不变量）
//! 每 render 帧：O(本 sim 区间在动 ∩ 存活) 的插值重算（A8 稀疏性经写日志延伸到连续
//! 更新）+ O(**可见**) 的连续扫与 `submit` 装配（启用剔除后存活扫的 N 被可见集收窄；
//! 未启用则退化 O(存活) 全扫）；每 sim 帧：O(|事件|) 的反应路由 + O(position 写) 的网格
//! 增量维护。

pub mod clock;
pub mod ctx;
pub mod handoff;
pub mod interp;
mod store;
pub mod submission;
pub mod visible;

pub use clock::RenderClock;
pub use ctx::{ContinuousFn, ReactionFn, RenderCtx, RenderInput};
pub use handoff::{Publisher, SimFrame, TrackedDelta};
pub use interp::{Interp, Track};
pub use store::RFieldId;
pub use submission::{
    RenderBinding, RenderPacket, SubmissionInstanceKey, SubmissionInstanceLayout,
    SubmissionInstanceRow, SubmissionInstanceSlot, SubmissionInstanceSpan, SubmissionInstanceStream,
    SubmissionView,
};
pub use visible::{Axes, CullShape, lod_band};

use std::collections::{HashMap, HashSet};

use crate::entity::{EntityTypeId, FieldId, InstanceId};
use crate::predicate::{Cond, Expr, Proj, ValRef};
use crate::runtime::{CompiledCond, Detect, Runtime, project_ro};
use crate::spatial::SpatialGrid;
use crate::value::Value;

use store::RenderStore;

struct Reaction {
    /// D1 / C5 检测消息里用作 owner 名。
    name: String,
    /// scope：`type(ty, sim_field)`。v1 订阅者 = writer 本身。
    ty: EntityTypeId,
    sim_field: FieldId,
    /// 预编译条件：复用 sim 的 [`CompiledCond`]（同一份扁平后缀求值器，走 `eval_ro`）。
    compiled: CompiledCond,
    projs: Vec<Proj>,
    batched: bool,
    writes: HashSet<RFieldId>,
    f: ReactionFn,
}

struct Continuous {
    /// D1 / C5 检测消息里用作 owner 名。
    name: String,
    ty: EntityTypeId,
    writes: HashSet<RFieldId>,
    f: ContinuousFn,
}

/// 一个被 `cull_type` opt-in 的剔除类型的登记。
#[derive(Clone, Copy)]
struct CulledType {
    /// 喂网格的 sim 平移字段（须已 `track`）。ingest 按 (ty, 此字段) 匹配增量喂入。
    translation_field: FieldId,
    /// 用于 render-frame 采样刷新网格的 track 槽位，确保剔除与提交 transform 同时刻。
    track_slot: usize,
    /// 可选：每帧把可见实体到相机距离写入此 render 字段（开发者据此分档 LOD）。
    dist_field: Option<RFieldId>,
}

/// render 侧空间索引 / 可见集剔除 / LOD 状态（§6.1「物化为索引实体」的 render 对偶）。
/// 全在一个 `Option` 里：`None` = 未启用剔除（行为与今日逐字相同）。
struct Spatial {
    /// render 私有网格（喂自 ingest 的 tracked position 增量；派生态，不入任何快照）。
    grid: SpatialGrid,
    /// 全局投影平面（Vec3 平移 → 网格 2D 平面）。相机与各剔除类型共用。
    axes: Axes,
    /// 相机实例 + 其位姿 render 字段（render-rate：由 track/continuous 维护）+ cull 形状。
    camera: InstanceId,
    cam_pos: RFieldId,
    shape: CullShape,
    /// 被剔除的类型登记。
    culled: HashMap<EntityTypeId, CulledType>,
    /// 本帧可见集（每个剔除类型一个桶，空桶 = 该类型本帧无可见实体）。
    visible: HashMap<EntityTypeId, Vec<InstanceId>>,
    /// 本帧剔除是否生效（相机在场且位姿可投影）。false ⇒ 消费侧退化全可见兜底。
    active: bool,
}

/// 第二个 runtime 实例：动态帧率的 render 侧。
pub struct RenderRuntime {
    store: RenderStore,
    tracks: Vec<Track>,
    /// (类型, sim 字段) → tracks 下标（可多个：同一 sim 字段可被多个 track 以不同
    /// 插值种类镜像；摄入增量时逐个喂，否则除最后注册者外全部停在默认值）。
    track_of: HashMap<(EntityTypeId, FieldId), Vec<usize>>,
    /// sim 字段默认值快照（构造期从 sim schema 取，出生 snap 用）。
    sim_defaults: HashMap<(EntityTypeId, FieldId), Value>,
    reactions: Vec<Reaction>,
    continuous: Vec<Continuous>,
    /// render 侧 D1：(类型, render 字段) → 归属者描述（注册期冲突即错）。
    field_owner: HashMap<(EntityTypeId, RFieldId), String>,
    /// C5 检测档位（与 sim 同一 [`Detect`]）：管「同 calc 多次运行对同 render 字段写
    /// 不同值」的折叠序未定义告警。默认跟随构建档（debug→Warn / release→Silent）。
    detect: Detect,
    clock: RenderClock,
    /// 已摄入的最新 sim 帧号（去重：同一 SimFrame 跨多个 render 帧只摄入一次）。
    last_ingested: u64,
    /// 本 sim 区间在动、需每 render 帧重算插值的 (实例, track 下标)（A8 稀疏集）。
    active: Vec<(InstanceId, usize)>,
    /// 可渲染类型的提交绑定（注册序）：[`submission::assemble`] 据此装配提交视图。
    renderables: Vec<(EntityTypeId, RenderBinding)>,
    /// render 自管寿命（死亡淡出）：类型 → (淡出权重字段, 淡出时长秒)。sim 写死后，
    /// 该类型实体不即时回收 render 行，而是按真实 dt 把权重 1→0 推过去，到 0 才回收。
    death_fade: HashMap<EntityTypeId, (RFieldId, f64)>,
    /// 正在淡出的尸体：(实例, 剩余秒)。每 render 帧 `-= dt` 并写淡出字段；≤0 回收行。
    /// sim 复用同 id 重生时，出生摄入按 (类型,id) 清除残项（重生即时夺回行）。
    dying: Vec<(InstanceId, f64)>,
    /// render 侧空间索引 / 可见集剔除 / LOD（`None` = 未启用，行为与今日相同）。
    spatial: Option<Spatial>,
}

impl RenderRuntime {
    /// 从 sim runtime 的 schema 构造（两侧共享 schema）。每个 sim 类型预留一个镜像，
    /// 并快照全字段默认值（出生 snap 用——render 只见写日志增量，未写出的 tracked
    /// 字段须从 schema 默认值取值）。
    pub fn new(sim: &Runtime) -> Self {
        let mut sim_defaults = HashMap::new();
        for t in 0..sim.type_count() {
            let ty = EntityTypeId(t as u32);
            for (fi, def) in sim.field_defaults(ty).into_iter().enumerate() {
                sim_defaults.insert((ty, FieldId(fi as u32)), def);
            }
        }
        RenderRuntime {
            store: RenderStore::new(sim.type_count()),
            tracks: vec![],
            track_of: HashMap::new(),
            sim_defaults,
            reactions: vec![],
            continuous: vec![],
            field_owner: HashMap::new(),
            detect: Detect::default(),
            clock: RenderClock::new(),
            last_ingested: 0,
            active: vec![],
            renderables: vec![],
            death_fade: HashMap::new(),
            dying: vec![],
            spatial: None,
        }
    }

    /// C5 检测档位（默认跟随构建档）。与 sim 的 [`Runtime::set_detect`] 对偶——令 render
    /// 的 D1 折叠冲突在 QA 下可 Strict panic，而非静默 last-wins。
    pub fn set_detect(&mut self, d: Detect) {
        self.detect = d;
    }

    // ---- 注册期 ----

    /// 在某类型注册一个纯 render 字段（render 独占写，D1 render 侧）。
    pub fn add_render_field(&mut self, ty: EntityTypeId, default: Value) -> RFieldId {
        self.store.add_render_field(ty, default)
    }

    /// 声明镜像某 sim 字段并按 `kind` 维护插值输出，返回插值结果所在的 render 字段。
    /// 这是 render 侧的 `fold`：runtime 每帧增量维护，连续 calc 直接读输出字段。
    ///
    /// 同一 sim 字段可被多次 track（不同插值种类各自一个输出字段）。输出字段经
    /// `claim_writes` 登记 D1 归属（与 reaction/continuous 同纪律，冲突即错）。
    pub fn track(
        &mut self,
        ty: EntityTypeId,
        sim_field: FieldId,
        kind: Interp,
    ) -> Result<RFieldId, String> {
        let default = self
            .sim_defaults
            .get(&(ty, sim_field))
            .cloned()
            .unwrap_or(Value::Null);
        // 输出字段按插值种类定型（Vec3Lerp→Vec3 / Slerp→Quat / Lerp→Float / Snap·Step→源型），
        // 而非恒 Null——否则输出列落 Boxed，render 最热的每帧 transform 插值输出丢去装箱收益。
        let out = self.store.add_render_field(ty, kind.out_default(&default));
        // 输出字段走 D1 登记（out 恒为新铸 id，但宿主可手构 RFieldId 抢注，故仍检查）。
        self.claim_writes(ty, &[out], &format!("track({}.{})", ty.0, sim_field.0))?;
        let slot = self.store.add_track(ty, sim_field, default);
        let idx = self.tracks.len();
        self.tracks.push(Track {
            ty,
            sim_field,
            out,
            slot,
            kind,
        });
        self.track_of.entry((ty, sim_field)).or_default().push(idx);
        Ok(out)
    }

    /// 注册 render 事件反应。`cond` 只许引用 new/old/常量（render 反应不点查订阅者
    /// 行——v1 约束；越界在此报错）。`projs` 只许 new/old/writer。
    #[allow(clippy::too_many_arguments)]
    pub fn reaction(
        &mut self,
        name: &str,
        ty: EntityTypeId,
        sim_field: FieldId,
        cond: Cond,
        projs: Vec<Proj>,
        batched: bool,
        writes: &[RFieldId],
        f: ReactionFn,
    ) -> Result<(), String> {
        validate_reaction_cond(&cond)?;
        validate_reaction_projs(&projs)?;
        self.claim_writes(ty, writes, name)?;
        // 复用 sim 的预编译条件：render 反应条件是 `Cond` 的只读子集（无 own/self），
        // 编成同一份扁平后缀程序，运行期走 `eval_ro`——render 不再 fork 求值器。
        let compiled = CompiledCond::compile(&cond);
        self.reactions.push(Reaction {
            name: name.to_string(),
            ty,
            sim_field,
            compiled,
            projs,
            batched,
            writes: writes.iter().copied().collect(),
            f,
        });
        Ok(())
    }

    /// 注册 render 连续更新：render clock 每帧对该类型每个存活实例运行一次。
    pub fn continuous(
        &mut self,
        name: &str,
        ty: EntityTypeId,
        writes: &[RFieldId],
        f: ContinuousFn,
    ) -> Result<(), String> {
        self.claim_writes(ty, writes, name)?;
        self.continuous.push(Continuous {
            name: name.to_string(),
            ty,
            writes: writes.iter().copied().collect(),
            f,
        });
        Ok(())
    }

    /// 声明某类型可渲染并绑定其提交字段（哪些 render 字段是 transform / handle /
    /// 可见性 / 动画态 / 淡出）。[`RenderRuntime::submit`] 据此装配提交视图。每类型
    /// 一份绑定，重复注册即错。绑定只引用已存在的 render 字段（track 输出 / 纯 render
    /// 字段），不另立写权——故不走 D1 登记。
    pub fn renderable(&mut self, ty: EntityTypeId, binding: RenderBinding) -> Result<(), String> {
        if self.renderables.iter().any(|(t, _)| *t == ty) {
            return Err(format!("类型 {} 已注册 renderable 绑定", ty.0));
        }
        self.validate_render_binding(ty, &binding)?;
        self.renderables.push((ty, binding));
        Ok(())
    }

    /// 装配本 render 帧的提交视图（staging 数据）：逐可渲染类型扫存活（含淡出中）
    /// 实体，读绑定字段，剔除不可见 / 已淡尽者，产出有序 [`RenderPacket`] 列。
    /// 只读——在 [`RenderRuntime::render_frame`] 之后调用，取走本帧渲染语义数据交后端。
    pub fn submit(&self) -> SubmissionView {
        // 剔除生效时把可见集传给装配器（剔除类型只装可见者）；否则 None = 全装（含相机
        // 缺席兜底、未启用剔除）。
        let visible = self
            .spatial
            .as_ref()
            .filter(|sp| sp.active)
            .map(|sp| &sp.visible);
        submission::assemble(&self.store, &self.renderables, visible)
    }

    /// 开启某类型的 render 自管死亡淡出：sim 写死后，render 不即时回收行，而是在
    /// `duration` 秒内把 `fade_field` 权重由 1 推到 0（按真实 dt 积分，非帧数——动态
    /// 帧率下淡出时长稳定），到 0 才真正回收。淡出期内实体仍在场于 render（`is_present`
    /// 为真、进提交、`continuous` 继续 tick——死亡动画照播）。
    ///
    /// `fade_field` 由淡出机制独占写（走 D1 登记，冲突即错），其 schema 默认值应为
    /// `Float(1.0)`（存活实体实心）。重复设同一类型即错。`duration` 须 `>0`。
    pub fn set_death_fade(
        &mut self,
        ty: EntityTypeId,
        fade_field: RFieldId,
        duration: f64,
    ) -> Result<(), String> {
        if !duration.is_finite() || duration <= 0.0 {
            return Err(format!("死亡淡出时长须为有限正数（给 {duration}）"));
        }
        if self.death_fade.contains_key(&ty) {
            return Err(format!("类型 {} 已设死亡淡出", ty.0));
        }
        if !self.store.has_render_field(ty, fade_field) {
            return Err(format!("死亡淡出字段 {}.{} 不存在", ty.0, fade_field.0));
        }
        self.claim_writes(ty, &[fade_field], &format!("death_fade({})", ty.0))?;
        self.death_fade.insert(ty, (fade_field, duration));
        Ok(())
    }

    /// 启用 render 侧剔除 / LOD（§6.1「物化为索引实体」的 render 对偶）：建一份 render
    /// 私有 [`SpatialGrid`]（`cell_size` ≥ 最大交互直径 / cull 形状跨度，见其文档），设
    /// 全局投影平面 `axes`（Vec3 平移取哪两分量入网格），designate 相机实例 `camera` +
    /// 其位姿 render 字段 `cam_pos`（须已存在；由 track/continuous 按 render-rate 维护）
    /// + cull 形状 `shape`。随后用 [`RenderRuntime::cull_type`] 把各类型 opt-in。
    ///
    /// 未调用此法时 render 行为与今日逐字相同（`continuous`/`submit` 扫全部存活）。
    /// 重复启用即错。
    pub fn enable_culling(
        &mut self,
        cell_size: f64,
        axes: Axes,
        camera: InstanceId,
        cam_pos: RFieldId,
        shape: CullShape,
    ) -> Result<(), String> {
        if self.spatial.is_some() {
            return Err("已启用剔除（enable_culling 只能调一次）".into());
        }
        if !(cell_size.is_finite() && cell_size > 0.0) {
            return Err(format!("cell_size 须为有限正数（给 {cell_size}）"));
        }
        if !shape.is_valid() {
            return Err("cull 形状参数须为有限正数".into());
        }
        if !self.store.has_render_field(camera.ty, cam_pos) {
            return Err(format!("相机位姿字段 {}.{} 不存在", camera.ty.0, cam_pos.0));
        }
        self.spatial = Some(Spatial {
            grid: SpatialGrid::new(cell_size),
            axes,
            camera,
            cam_pos,
            shape,
            culled: HashMap::new(),
            visible: HashMap::new(),
            active: false,
        });
        Ok(())
    }

    /// 把某类型 opt-in 进 render 剔除：其 `translation_sim_field`（**须已 [`track`]**，
    /// 否则无 tracked 增量喂网格——此处即错）的位喂入网格，该类型的 `continuous` /
    /// `submit` 每帧收窄到相机视域内的可见集。`dist_field` 可选——给定则每帧把可见
    /// 实体到相机的距离写入它（走 `claim_writes` D1 登记，runtime 独占写，与 track 输出 /
    /// fade 同纪律），供开发者读取 + [`lod_band`] 自行分档 LOD。须先 `enable_culling`。
    /// 重复 cull 同类型即错。
    ///
    /// [`track`]: RenderRuntime::track
    pub fn cull_type(
        &mut self,
        ty: EntityTypeId,
        translation_sim_field: FieldId,
        dist_field: Option<RFieldId>,
    ) -> Result<(), String> {
        if self.spatial.is_none() {
            return Err("须先 enable_culling 再 cull_type".into());
        }
        if !self.track_of.contains_key(&(ty, translation_sim_field)) {
            return Err(format!(
                "cull_type 要求 {}.{} 已被 track（剔除靠 tracked position 增量喂网格）",
                ty.0, translation_sim_field.0
            ));
        }
        if self.spatial.as_ref().unwrap().culled.contains_key(&ty) {
            return Err(format!("类型 {} 已 cull", ty.0));
        }
        // dist_field 走 D1 登记（runtime 独占写）。先登记再插登记表，错误路径不留脏态。
        if let Some(df) = dist_field {
            self.claim_writes(ty, &[df], &format!("visible_set_dist({})", ty.0))?;
        }
        let track_slot = self
            .track_of
            .get(&(ty, translation_sim_field))
            .and_then(|tis| tis.first())
            .map(|&ti| self.tracks[ti].slot)
            .expect("cull_type 已验证 translation_sim_field 被 track");
        let axes = self.spatial.as_ref().unwrap().axes;
        let mut live = vec![];
        self.store.for_each_live(ty, |inst| live.push(inst));
        let mut backfill = vec![];
        for inst in live {
            if let Some((_prev, cur)) = self.store.track_pair(inst, track_slot)
                && let Some((x, y)) = axes.project(&cur)
            {
                backfill.push((inst, x, y));
            }
        }
        self.spatial.as_mut().unwrap().culled.insert(
            ty,
            CulledType {
                translation_field: translation_sim_field,
                track_slot,
                dist_field,
            },
        );
        let sp = self.spatial.as_mut().unwrap();
        sp.visible.entry(ty).or_default();
        for (inst, x, y) in backfill {
            sp.grid.update(inst, x, y);
        }
        Ok(())
    }

    fn validate_render_binding(
        &self,
        ty: EntityTypeId,
        binding: &RenderBinding,
    ) -> Result<(), String> {
        if !self.store.has_type(ty) {
            return Err(format!("无类型 id {}", ty.0));
        }
        let mut seen = HashSet::new();
        for (slot, field) in render_binding_fields(binding) {
            let Some(f) = field else { continue };
            if !self.store.has_render_field(ty, f) {
                return Err(format!(
                    "renderable 绑定 {slot} 引用不存在字段 {}.{}",
                    ty.0, f.0
                ));
            }
            if !seen.insert(f) {
                return Err(format!("renderable 重复绑定 render 字段 {}.{}", ty.0, f.0));
            }
        }
        Ok(())
    }

    /// D1 render 侧：render 字段静态归属唯一写者，注册期冲突即错。
    /// 先整片校验（含片内重复）再插入——失败时不留半截脏归属（错误路径整洁）。
    fn claim_writes(
        &mut self,
        ty: EntityTypeId,
        writes: &[RFieldId],
        owner: &str,
    ) -> Result<(), String> {
        for (i, &w) in writes.iter().enumerate() {
            if !self.store.has_render_field(ty, w) {
                return Err(format!("{owner} 声明不存在 render 字段 {}.{}", ty.0, w.0));
            }
            if let Some(prev) = self.field_owner.get(&(ty, w)) {
                return Err(format!(
                    "D1（render 侧）冲突：render 字段 {}.{} 已归属 {prev}",
                    ty.0, w.0
                ));
            }
            if writes[..i].contains(&w) {
                return Err(format!("{owner} 重复声明 render 字段 {}.{}", ty.0, w.0));
            }
        }
        for &w in writes {
            self.field_owner.insert((ty, w), owner.to_string());
        }
        Ok(())
    }

    /// 本 render runtime 关心的 tracked (类型, sim 字段) 集——用于构建 [`Publisher`]。
    pub fn tracked_fields(&self) -> Vec<(EntityTypeId, FieldId)> {
        self.tracks.iter().map(|t| (t.ty, t.sim_field)).collect()
    }

    // ---- 帧循环 ----

    /// 摄入一份 sim 快照（新于已摄入帧才生效；同一 SimFrame 跨多 render 帧只摄入
    /// 一次）。顺序：出生 → tracked 增量（含停动结算）→ 事件反应 → 死亡。
    pub fn ingest(&mut self, sf: &SimFrame) {
        if sf.sim_frame <= self.last_ingested {
            return;
        }
        // 1. 出生：行须先于增量/反应存在。birth 已把每个 track 的 prev/cur snap 到
        //    sim 默认值；这里把输出字段也 snap 一次（未被 init 写入的 tracked 字段否则
        //    永远停在 Null——若同帧 init 携带了值，下面的 apply_delta 会再覆盖）。
        for &inst in &sf.births {
            // 重生即时夺回行：清除该 (类型,id) 残留的网格/淡出/活动项。须在 birth
            // 重置 render 行之前做，因为 same-frame destroy+spawn 的旧代际此刻还可能
            // 是当前行；若先切到新代际，随后旧代际 death-fade 分支会因不在场而无法清理。
            if self.spatial.is_some() {
                let stale: Vec<InstanceId> = self
                    .dying
                    .iter()
                    .filter(|(d, _)| d.ty == inst.ty && d.id == inst.id)
                    .map(|(d, _)| *d)
                    .collect();
                if let Some(sp) = self.spatial.as_mut() {
                    sp.grid.remove_slot(inst);
                    for d in stale {
                        sp.grid.remove(d);
                    }
                }
            }
            self.store.birth(inst);
            self.dying
                .retain(|(d, _)| !(d.ty == inst.ty && d.id == inst.id));
            self.active
                .retain(|(a, _)| !(a.ty == inst.ty && a.id == inst.id));
            for ti in 0..self.tracks.len() {
                let tr = self.tracks[ti];
                if tr.ty != inst.ty {
                    continue;
                }
                if self.store.track_pair(inst, tr.slot).is_some() {
                    let default = self
                        .sim_defaults
                        .get(&(tr.ty, tr.sim_field))
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.store
                        .write_render(inst, tr.out, tr.kind.out_default(&default));
                }
            }
            // 出生即入网格（剔除类型）：按 sim 默认平移投影喂入，否则「出生后从不写
            // position」的静止实体永不进网格 = 永远被剔除。随后 position 增量再 refine。
            let feed = self.spatial.as_ref().and_then(|sp| {
                sp.culled
                    .get(&inst.ty)
                    .map(|ct| (ct.translation_field, sp.axes))
            });
            if let Some((tf, axes)) = feed {
                let def = self
                    .sim_defaults
                    .get(&(inst.ty, tf))
                    .cloned()
                    .unwrap_or(Value::Null);
                if let Some((x, y)) = axes.project(&def) {
                    self.spatial.as_mut().unwrap().grid.update(inst, x, y);
                }
            }
        }
        let births: HashSet<InstanceId> = sf.births.iter().copied().collect();
        // 2. tracked 增量 → 镜像 + 活动集（停动的 cell 结算到 cur）。同一 sim 字段的
        //    全部 track 都喂（track_of 是多值），否则除最后注册者外都停在默认值。
        let mut deltas: Vec<TrackedDelta> = vec![];
        let mut delta_of: HashMap<(InstanceId, FieldId), usize> = HashMap::new();
        for d in &sf.tracked {
            let key = (d.inst, d.sim_field);
            if let Some(&i) = delta_of.get(&key) {
                deltas[i].new = d.new.clone();
            } else {
                delta_of.insert(key, deltas.len());
                deltas.push(d.clone());
            }
        }
        let prev_active = std::mem::take(&mut self.active);
        let mut new_active: Vec<(InstanceId, usize)> = vec![];
        let mut new_set: HashSet<(InstanceId, usize)> = HashSet::new();
        for d in &deltas {
            if let Some(tis) = self.track_of.get(&(d.inst.ty, d.sim_field)) {
                let just_born = births.contains(&d.inst);
                for &ti in tis {
                    let slot = self.tracks[ti].slot;
                    let applied = self.store.apply_delta(
                        d.inst,
                        slot,
                        d.old.clone(),
                        d.new.clone(),
                        just_born,
                    );
                    if applied && new_set.insert((d.inst, ti)) {
                        new_active.push((d.inst, ti));
                    }
                }
            }
            // 喂网格（剔除类型的平移增量 → 把住户更新到最新 sim 位 cur）。仅对在场实体，
            // 避免已死 / 未生的陈旧增量造幽灵住户。
            let feed_axes = self.spatial.as_ref().and_then(|sp| {
                sp.culled
                    .get(&d.inst.ty)
                    .filter(|ct| ct.translation_field == d.sim_field)
                    .map(|_| sp.axes)
            });
            if let Some(axes) = feed_axes {
                if self.store.is_present(d.inst) {
                    if let Some((x, y)) = axes.project(&d.new) {
                        self.spatial.as_mut().unwrap().grid.update(d.inst, x, y);
                    } else {
                        self.spatial.as_mut().unwrap().grid.remove(d.inst);
                    }
                } else {
                    self.spatial.as_mut().unwrap().grid.remove(d.inst);
                }
            }
        }
        for (inst, ti) in prev_active {
            if !new_set.contains(&(inst, ti)) {
                // 上一区间在动、本区间不动：把插值输出结算到静止位（= 上一区间 alpha=1
                // 的取值，cur）。经 kind.sample 求值，保证与活动扫输出同型（Lerp 出 Float）。
                let tr = self.tracks[ti];
                if let Some((prev, cur)) = self.store.track_pair(inst, tr.slot) {
                    self.store
                        .write_render(inst, tr.out, tr.kind.sample(&prev, &cur, 1.0));
                }
            }
        }
        self.active = new_active;
        // 3. 事件反应（订阅者 = writer，行仍存活；先于死亡结算）。
        self.run_reactions(sf);
        // 4. 死亡：有淡出策略的类型 render 接管寿命（进入 dying，行暂留）；否则即时回收。
        for &inst in &sf.deaths {
            if let Some(&(_ff, dur)) = self.death_fade.get(&inst.ty) {
                if !self.push_dying(inst, dur)
                    && let Some(sp) = self.spatial.as_mut()
                {
                    sp.grid.remove(inst);
                }
            } else {
                self.store.death(inst);
                self.active.retain(|(a, _)| *a != inst);
                // 即时死亡 → 从网格移除（淡出类型留到淡尽回收时移，见 advance_dying）。
                if let Some(sp) = self.spatial.as_mut() {
                    sp.grid.remove(inst);
                }
            }
        }
        self.last_ingested = sf.sim_frame;
    }

    /// 推进一个 render 帧：更新时钟 → 插值重算（仅活动集，A8）→ 连续更新。
    pub fn render_frame(&mut self, dt: f64, alpha: f64) {
        self.clock.begin_frame(dt, alpha);
        let alpha = self.clock.alpha;
        // 插值扫：只碰本区间在动的 cell（稀疏）。静止物的输出字段已在结算时落到 cur。
        let active = std::mem::take(&mut self.active);
        let mut still_active = Vec::with_capacity(active.len());
        for (inst, ti) in active {
            let tr = self.tracks[ti];
            if let Some((prev, cur)) = self.store.track_pair(inst, tr.slot) {
                let out = tr.kind.sample(&prev, &cur, alpha);
                let cull_axes = self.spatial.as_ref().and_then(|sp| {
                    sp.culled
                        .get(&inst.ty)
                        .filter(|ct| ct.track_slot == tr.slot)
                        .map(|_| sp.axes)
                });
                if let Some(axes) = cull_axes {
                    if let Some((x, y)) = axes.project(&out) {
                        self.spatial.as_mut().unwrap().grid.update(inst, x, y);
                    } else {
                        self.spatial.as_mut().unwrap().grid.remove(inst);
                    }
                }
                self.store.write_render(inst, tr.out, out);
                still_active.push((inst, ti));
            }
        }
        self.active = still_active;
        self.advance_dying(self.clock.dt);
        // 算可见集（相机 render-rate 查询网格）+ 写距离字段，再据此收窄 continuous。
        // 置于插值扫之后：相机位姿若由 track 驱动，本帧已插值到位（零延迟）；若由
        // continuous 驱动则用上一帧相机位（1 帧延迟，cull 余量吸收）。未启用剔除时空操作。
        self.compute_visible();
        self.run_continuous();
    }

    fn push_dying(&mut self, inst: InstanceId, duration: f64) -> bool {
        if !self.store.is_present(inst) {
            return false;
        }
        if let Some((_, remaining)) = self.dying.iter_mut().find(|(d, _)| *d == inst) {
            *remaining = remaining.min(duration);
            return true;
        }
        self.dying.push((inst, duration));
        true
    }

    /// 推进死亡淡出：每 render 帧对淡出中的尸体 `剩余 -= dt`，写淡出权重
    /// `(剩余/时长)∈[0,1]`，剩余 ≤0 则真正回收 render 行。take 出本地后处理，
    /// 避免 `self.store` 可变借用与 `self.dying` 借用交叠。
    fn advance_dying(&mut self, dt: f64) {
        if self.dying.is_empty() {
            return;
        }
        let dying = std::mem::take(&mut self.dying);
        let mut still = Vec::with_capacity(dying.len());
        let mut reclaimed = vec![];
        for (inst, remaining) in dying {
            if !self.store.is_present(inst) {
                continue;
            }
            let remaining = remaining - dt;
            if let Some(&(ff, dur)) = self.death_fade.get(&inst.ty) {
                let w = (remaining / dur).clamp(0.0, 1.0);
                self.store.write_render(inst, ff, Value::Float(w));
            }
            if remaining > 0.0 {
                still.push((inst, remaining));
            } else {
                self.store.death(inst); // 淡尽：真正回收行（render 寿命终结）。
                reclaimed.push(inst);
                // 淡尽回收 → 从网格移除（淡出窗口内一直留在网格，故淡出期仍进可见集、照画）。
                if let Some(sp) = self.spatial.as_mut() {
                    sp.grid.remove(inst);
                }
            }
        }
        for inst in reclaimed {
            self.active.retain(|(a, _)| *a != inst);
        }
        self.dying = still;
    }

    /// 规范宿主入口：取走 publisher 全部未消费帧、顺序摄入（不丢生灭/事件），再推进
    /// 一个 render 帧。单线程与并发双线程的每个 render 帧都调它即可。若要在两个 sim
    /// 帧之间画多个插值帧，调一次 `sync` 后续接若干 [`RenderRuntime::render_frame`]
    /// （只变 alpha、不再摄入）。
    pub fn sync(&mut self, publisher: &Publisher, dt: f64, alpha: f64) {
        for sf in publisher.drain() {
            self.ingest(&sf);
        }
        self.render_frame(dt, alpha);
    }

    /// 算本 render 帧的可见集（§6.1 render 对偶的查询端）：读相机位姿、对网格做形状查询、
    /// 按剔除类型分桶，并把每个可见实体到相机的距离写入其 `dist_field`（若配置）。相机
    /// 缺席 / 位姿不可投影 ⇒ `active = false`，消费侧退化为全可见（避免黑屏）。未启用剔除
    /// ⇒ 直接返回。
    fn compute_visible(&mut self) {
        // 先把 Copy 配置取出，避免后续与 grid / visible / store 借用打架。
        let Some((camera, cam_pos, axes, shape)) = self
            .spatial
            .as_ref()
            .map(|sp| (sp.camera, sp.cam_pos, sp.axes, sp.shape))
        else {
            return;
        };
        // 1. 重置上帧可见集：每个剔除类型清空桶（保留容量），active 暂置 false。空桶 ≠ 无桶：
        //    剔除类型即使本帧 0 可见也须有桶，否则消费侧 `get(ty)=None` 会误退化为全扫。
        {
            let sp = self.spatial.as_mut().unwrap();
            let tys: Vec<EntityTypeId> = sp.culled.keys().copied().collect();
            for ty in tys {
                sp.visible.entry(ty).or_default().clear();
            }
            sp.active = false;
        }
        // 2. 相机投影点（缺席 / 不可投影 → active 留 false：消费侧全可见兜底）。
        if !self.store.is_present(camera) {
            return;
        }
        let cam_val = self.store.read_render(camera, cam_pos);
        let Some((cx, cy)) = axes.project(&cam_val) else {
            return;
        };
        // 3. 形状查询网格（含各剔除类型住户，确定序）。
        let hits = {
            let sp = self.spatial.as_ref().unwrap();
            shape.query(&sp.grid, cx, cy)
        };
        let mut live_hits = vec![];
        let mut stale_hits = vec![];
        for inst in hits {
            if self.store.is_present(inst) {
                live_hits.push(inst);
            } else {
                stale_hits.push(inst);
            }
        }
        // 4. 分桶 + 收集距离写（距离用网格存的 cur 位，与 cull 成员判定同源）。
        let mut dist_writes: Vec<(InstanceId, RFieldId, f64)> = vec![];
        {
            let sp = self.spatial.as_mut().unwrap();
            for inst in stale_hits {
                sp.grid.remove(inst);
            }
            for inst in live_hits {
                let Some(ct) = sp.culled.get(&inst.ty).copied() else {
                    continue; // 网格里只该有剔除类型，稳妥起见跳过未登记者。
                };
                if let Some(df) = ct.dist_field {
                    let dist = sp
                        .grid
                        .position(inst)
                        .map_or(0.0, |(ex, ey)| (ex - cx).hypot(ey - cy));
                    dist_writes.push((inst, df, dist));
                }
                sp.visible.entry(inst.ty).or_default().push(inst);
            }
            sp.active = true;
        }
        // 5. 距离写进各可见实体的 dist_field（runtime 独占写，D1 已登记）。
        for (inst, df, dist) in dist_writes {
            self.store.write_render(inst, df, Value::Float(dist));
        }
    }

    fn run_continuous(&mut self) {
        let detect = self.detect;
        for ci in 0..self.continuous.len() {
            let ty = self.continuous[ci].ty;
            let name = self.continuous[ci].name.clone();
            let declared = self.continuous[ci].writes.clone();
            // 剔除类型且本帧剔除生效 → 只扫可见集（离屏实体派生 render 态冻结，回屏即续，
            // 正是剔除本意）；否则（未剔除 / 相机缺席兜底）稠密扫存活。
            let mut insts = vec![];
            let narrowed = self
                .spatial
                .as_ref()
                .is_some_and(|sp| sp.active && sp.culled.contains_key(&ty));
            if narrowed {
                if let Some(vis) = self.spatial.as_ref().unwrap().visible.get(&ty) {
                    insts.extend_from_slice(vis);
                }
                insts.retain(|&inst| self.store.is_present(inst));
            } else {
                self.store.for_each_live(ty, |i| insts.push(i));
            }
            for inst in insts {
                let writes = {
                    let mut ctx = RenderCtx {
                        store: &self.store,
                        self_id: inst,
                        clock: self.clock,
                        writes: vec![],
                    };
                    (self.continuous[ci].f)(&mut ctx);
                    ctx.writes
                };
                commit_writes(&mut self.store, inst, &declared, &name, detect, writes);
            }
        }
    }

    fn run_reactions(&mut self, sf: &SimFrame) {
        let detect = self.detect;
        let deaths: HashSet<InstanceId> = sf.deaths.iter().copied().collect();
        for ri in 0..self.reactions.len() {
            let ty = self.reactions[ri].ty;
            let sim_field = self.reactions[ri].sim_field;
            let batched = self.reactions[ri].batched;
            let name = self.reactions[ri].name.clone();
            let declared = self.reactions[ri].writes.clone();
            // 收集命中（先收集再运行，避开借用交叠）。条件复用 sim 预编译求值器
            // （`eval_ro`），投影复用 `project_ro`——render 不再持有自己的副本。
            let mut eval_stack: Vec<Value> = vec![];
            let mut hits: Vec<(InstanceId, Vec<Value>)> = vec![];
            for rec in &sf.events {
                if deaths.contains(&rec.inst) {
                    continue;
                }
                if rec.inst.ty == ty
                    && rec.field == sim_field
                    && self.reactions[ri]
                        .compiled
                        .eval_ro(&rec.new, &rec.old, &mut eval_stack)
                {
                    hits.push((rec.inst, project_ro(&self.reactions[ri].projs, rec)));
                }
            }
            if batched {
                let mut groups: HashMap<InstanceId, Vec<Vec<Value>>> = HashMap::new();
                let mut order: Vec<InstanceId> = vec![];
                for (inst, row) in hits {
                    groups.entry(inst).or_insert_with(|| {
                        order.push(inst);
                        vec![]
                    });
                    groups.get_mut(&inst).unwrap().push(row);
                }
                for inst in order {
                    if !self.store.is_present(inst) {
                        continue;
                    }
                    let rows = groups.remove(&inst).unwrap();
                    let writes = {
                        let mut ctx = RenderCtx {
                            store: &self.store,
                            self_id: inst,
                            clock: self.clock,
                            writes: vec![],
                        };
                        (self.reactions[ri].f)(&mut ctx, &RenderInput::Batch(rows));
                        ctx.writes
                    };
                    commit_writes(&mut self.store, inst, &declared, &name, detect, writes);
                }
            } else {
                for (inst, row) in hits {
                    if !self.store.is_present(inst) {
                        continue;
                    }
                    let writes = {
                        let mut ctx = RenderCtx {
                            store: &self.store,
                            self_id: inst,
                            clock: self.clock,
                            writes: vec![],
                        };
                        (self.reactions[ri].f)(&mut ctx, &RenderInput::Each(row));
                        ctx.writes
                    };
                    commit_writes(&mut self.store, inst, &declared, &name, detect, writes);
                }
            }
        }
    }

    // ---- 检视 ----

    /// 读某实例的 render 字段（检视 / 提交给 GPU 前取值）。
    pub fn read(&self, inst: InstanceId, f: RFieldId) -> Value {
        self.store.read_render(inst, f)
    }

    /// 该实例在 render 侧是否在场。语义 ≠ sim `_alive`：死亡淡出窗口内仍在场
    /// （render 自管寿命未回收），故 render「在场」= sim 存活 ⊔ 淡出尸体。
    pub fn is_present(&self, inst: InstanceId) -> bool {
        self.store.is_present(inst)
    }

    pub fn clock(&self) -> RenderClock {
        self.clock
    }

    /// 已摄入的最新 sim 帧号（检视握手进度）。
    pub fn last_ingested(&self) -> u64 {
        self.last_ingested
    }

    /// 当前「本区间在动」的活动 cell 数（检视 A8 稀疏性：静止场景应趋零）。
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// 当前正在死亡淡出（render 接管寿命、尚未回收）的尸体数。
    pub fn dying_count(&self) -> usize {
        self.dying.len()
    }
}

fn render_binding_fields(binding: &RenderBinding) -> [(&'static str, Option<RFieldId>); 9] {
    [
        ("translation", binding.translation),
        ("rotation", binding.rotation),
        ("scale", binding.scale),
        ("mesh", binding.mesh),
        ("material", binding.material),
        ("visibility", binding.visibility),
        ("anim_state", binding.anim_state),
        ("anim_phase", binding.anim_phase),
        ("fade", binding.fade),
    ]
}

/// 写折叠（render 侧）：同 calc 一次运行对同 render 字段的多次写取最终值；D1 校验。
/// 检测纪律与 sim `run_triggers` 一致：未声明写恒 panic（D1 render 侧）；同字段多写
/// 不同值按 C5 [`Detect`] 档告警（Strict panic / Warn eprintln / Silent 静默折叠）
/// ——render 不再独有「永远静默 last-wins」的弱策略。
fn commit_writes(
    store: &mut RenderStore,
    inst: InstanceId,
    declared: &HashSet<RFieldId>,
    owner: &str,
    detect: Detect,
    writes: Vec<(RFieldId, Value)>,
) {
    let mut folded: Vec<(RFieldId, Value)> = vec![];
    for (f, v) in writes {
        // 硬断言（与 sim 引擎 run_triggers 同纪律）：未声明写破坏 D1 render 侧。
        assert!(
            declared.contains(&f),
            "render calc {owner} 写了未声明的 render 字段 {}（D1 render 侧要求静态写集）",
            f.0
        );
        if let Some(slot) = folded.iter_mut().find(|(ff, _)| *ff == f) {
            // C5 检测：同 calc 多次运行对同字段写不同值，折叠序未定义（与 sim 同纪律）。
            if slot.1 != v && detect != Detect::Silent {
                let msg = format!(
                    "[PCE-render] {owner} 多次运行对同 render 字段 {} 写入不同值，折叠序未定义",
                    f.0
                );
                if detect == Detect::Strict {
                    panic!("{msg}");
                }
                eprintln!("{msg}");
            }
            slot.1 = v;
        } else {
            folded.push((f, v));
        }
    }
    for (f, v) in folded {
        store.write_render(inst, f, v);
    }
}

// ---- 反应条件 / 投影准入校验（v1 子集：new / old / 常量；求值复用 route 的预编译器）----

fn validate_reaction_cond(c: &Cond) -> Result<(), String> {
    fn expr_ok(e: &Expr) -> bool {
        match e {
            Expr::Val(v) => !matches!(v, ValRef::Own(_) | ValRef::SelfRef),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                expr_ok(a) && expr_ok(b)
            }
        }
    }
    let ok = match c {
        Cond::True | Cond::InRange(..) | Cond::InSet(_) | Cond::Changed | Cond::Became(_) => true,
        Cond::Cmp(a, _, b) => expr_ok(a) && expr_ok(b),
        Cond::Crossed(t, _) => expr_ok(t),
        Cond::And(a, b) | Cond::Or(a, b) | Cond::AndNot(a, b) => {
            return validate_reaction_cond(a).and(validate_reaction_cond(b));
        }
    };
    if ok {
        Ok(())
    } else {
        Err("render 反应条件 v1 只许引用 new/old/常量（不点查订阅者行）".into())
    }
}

fn validate_reaction_projs(projs: &[Proj]) -> Result<(), String> {
    for p in projs {
        if matches!(p, Proj::Own(_)) {
            return Err("render 反应投影 v1 只许 new/old/writer（不投影 own）".into());
        }
    }
    Ok(())
}

// 反应条件求值 / 投影现复用 sim 的预编译器（[`CompiledCond::eval_ro`] / [`project_ro`]）：
// render 条件是 `Cond` 的只读子集（无 own/self，上面 validate_* 注册期保证），故能直接
// 走同一份扁平后缀程序，不再维护第二份树游走求值器与投影器（条件语义单一真源）。
