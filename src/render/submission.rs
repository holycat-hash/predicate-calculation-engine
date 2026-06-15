//! GPU 提交流程：把 render 的逐实体语义字段装配成一份**有序 staging 视图**，
//! 供（未来的）GPU 后端打包成顶点 / 实例缓冲。
//!
//! 设计立场（架构哲学：白送做满 / 有代价给接口 / 不替开发者决策）：真正的 GPU
//! 后端——缓冲布局、批次合并、residency、重型光追 / 3D 纹理——是「有代价」优化，
//! 该交的是**接口 + 良好上游**而非替开发者拍板的实现。故本模块只产出「良好的渲染
//! 语义数据」：每个可见实体一条 [`RenderPacket`]（插值后的 transform、mesh / material
//! handle、动画态、淡出权重），后端据此自取所需地打包。
//!
//! 装配源全是 render 字段（[`RFieldId`]）：transform 经 `track` 插值落字段、
//! handle / 可见性 / 动画态经 `reaction` / `continuous` 写字段。`submit` 只读不写，是
//! render 帧的终端读出（§6.1「物化为可见集实体」的消费端：剔除把可见集收窄，提交
//! 只扫可见集）。

use std::collections::HashMap;

use crate::entity::{EntityTypeId, InstanceId};
use crate::value::Value;

use super::store::{RFieldId, RenderStore};

/// 某可渲染类型的字段绑定：声明哪些 render 字段填充提交包的各槽。未绑定的槽在包里
/// 取 `Null`（transform / handle 系）或默认（`anim_phase=0`、`fade=1`、可见）。
///
/// 一处声明、`submit` 处复用——避免把「字段语义」散落在每个消费点。各槽相互独立：
/// 平移走 `Vec3Lerp` track、旋转走 `Slerp` track、缩放走 `Vec3Lerp`、handle 走离散
/// render 字段，互不耦合。
#[derive(Debug, Clone, Default)]
pub struct RenderBinding {
    /// 平移（[`Value::Vec3`]，一般经 `Vec3Lerp` track）。
    pub translation: Option<RFieldId>,
    /// 旋转（[`Value::Quat`]，一般经 `Slerp` track）。
    pub rotation: Option<RFieldId>,
    /// 缩放（[`Value::Vec3`]，一般经 `Vec3Lerp` track）。
    pub scale: Option<RFieldId>,
    /// mesh handle（不透明值，离散——`Snap` track 或直接 render 字段）。
    pub mesh: Option<RFieldId>,
    /// material handle（不透明值）。
    pub material: Option<RFieldId>,
    /// 可见性（Bool）。未绑定 ⇒ 恒可见；读到 `Bool(false)` ⇒ 剔除（不进提交）。
    pub visibility: Option<RFieldId>,
    /// 动画状态 id（离散）。
    pub anim_state: Option<RFieldId>,
    /// 动画归一化进度 `[0,1)`（Float）。
    pub anim_phase: Option<RFieldId>,
    /// 淡出权重 `[0,1]`（Float）。未绑定 ⇒ 1（实心）；读到 `≤0` ⇒ 已淡尽，不进提交。
    pub fade: Option<RFieldId>,
}

/// 一个可见实体的提交包：装配好的逐实体渲染语义数据。后端把它打包成 draw / instance。
#[derive(Debug, Clone, PartialEq)]
pub struct RenderPacket {
    pub inst: InstanceId,
    /// 插值后的平移（[`Value::Vec3`]）/ 旋转（[`Value::Quat`]）/ 缩放（[`Value::Vec3`]）。
    /// 未绑定者为 `Null`（后端按缺省 TRS 处理）。三者分量独立，后端自行合成矩阵。
    pub translation: Value,
    pub rotation: Value,
    pub scale: Value,
    pub mesh: Value,
    pub material: Value,
    pub anim_state: Value,
    pub anim_phase: f64,
    /// 淡出权重 `[0,1]`：1 实心、0 淡尽（淡尽者已被排除，故恒 `>0`）。
    pub fade: f64,
}

/// 一个 render 帧的提交视图：按类型注册序、类型内按行序排列的提交包（稳定序）。
/// 批次 / 排序 / 缓冲打包交给后端（不替开发者决策）。
#[derive(Debug, Clone, Default)]
pub struct SubmissionView {
    pub packets: Vec<RenderPacket>,
}

impl SubmissionView {
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, RenderPacket> {
        self.packets.iter()
    }
}

/// 从一组渲染绑定装配提交视图。逐类型按注册序、类型内按 render 行序扫描存活
/// （含淡出中的）实体，读绑定字段，剔除不可见 / 已淡尽者。
///
/// `visible`（[`super::RenderRuntime`] 本帧算出的可见集）若为 `Some` 且含某类型，则该
/// 类型只扫可见集（空间剔除收窄候选，§6.1 的 render 对偶）；否则该类型扫全部存活。空间
/// 剔除与逐实体的 `visibility` / `fade` 字段正交叠加——前者收窄候选，后者再细筛。
pub(super) fn assemble(
    store: &RenderStore,
    renderables: &[(EntityTypeId, RenderBinding)],
    visible: Option<&HashMap<EntityTypeId, Vec<InstanceId>>>,
) -> SubmissionView {
    let mut packets = vec![];
    for (ty, b) in renderables {
        // 剔除类型（可见集含该 ty）只扫可见集；否则稠密扫存活（含淡出中）。
        let insts: Vec<InstanceId> = match visible.and_then(|m| m.get(ty)) {
            Some(vis) => vis.clone(),
            None => {
                let mut v = vec![];
                store.for_each_live(*ty, |i| v.push(i));
                v
            }
        };
        for inst in insts {
            if !store.is_present(inst) {
                continue;
            }
            // 剔除：绑定了可见性且读到 false。未绑定 / Null / true 一律可见（宽容默认）。
            if let Some(vf) = b.visibility
                && matches!(store.read_render(inst, vf), Value::Bool(false))
            {
                continue;
            }
            let fade = match b.fade {
                None => 1.0,
                Some(f) => match store.read_render(inst, f).as_f64() {
                    Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
                    _ => 0.0,
                },
            };
            if fade <= 0.0 {
                continue; // 已淡尽：本帧不画（行将由 render 寿命管理回收）。
            }
            packets.push(RenderPacket {
                inst,
                translation: read_opt(store, inst, b.translation),
                rotation: read_opt(store, inst, b.rotation),
                scale: read_opt(store, inst, b.scale),
                mesh: read_opt(store, inst, b.mesh),
                material: read_opt(store, inst, b.material),
                anim_state: read_opt(store, inst, b.anim_state),
                anim_phase: b
                    .anim_phase
                    .map_or(0.0, |f| store.read_render(inst, f).as_f64().unwrap_or(0.0)),
                fade,
            });
        }
    }
    SubmissionView { packets }
}

fn read_opt(store: &RenderStore, inst: InstanceId, f: Option<RFieldId>) -> Value {
    f.map_or(Value::Null, |f| store.read_render(inst, f))
}
