//! render calculation 的执行上下文。
//!
//! 与 sim 的 [`crate::Ctx`] 同形，但权限被裁剪以契合 render 的消费者角色：
//! - 读：自己（及任意实例）的 **render 字段**；sim 字段经 tracked 镜像已物化为
//!   render 字段（插值输出 / Snap 输出），故一律走 render 字段读。
//! - 写：仅自己实例的 render 字段（写局部性 + D1 render 侧）。
//! - **无 `spawn` / `destroy`**：共享实体的生杀只在 sim 侧（你的约束）。render
//!   连一个能创建共享实体的入口都不暴露——非法状态无法被写出来。
//! - 时间：读 `dt` / `alpha`（动态帧率），不读「帧数当时长」。

use crate::entity::InstanceId;
use crate::value::Value;

use super::clock::RenderClock;
use super::store::{RFieldId, RenderStore};

/// 谓词交付给 render reaction 的输入。只镜像 render 真实支持的两种交付势
/// （each / batch）；continuous 不经此通道——它由 render clock 直驱，读 [`RenderCtx`]
/// 的时钟、无投影输入（故无 `Tick` 势：那曾是从未被构造的死变体）。
#[derive(Debug, Clone)]
pub enum RenderInput {
    /// each：一条命中的投影元组（sim 写的快照投影）。
    Each(Vec<Value>),
    /// batch：整 sim 帧命中的多重集（顺序未定义，D3）。
    Batch(Vec<Vec<Value>>),
}

impl RenderInput {
    pub fn args(&self) -> &[Value] {
        match self {
            RenderInput::Each(v) => v,
            _ => panic!("RenderInput::args 仅适用于 each 反应"),
        }
    }

    pub fn arg(&self, i: usize) -> &Value {
        &self.args()[i]
    }

    pub fn rows(&self) -> &[Vec<Value>] {
        match self {
            RenderInput::Batch(v) => v,
            _ => panic!("RenderInput::rows 仅适用于 batch 反应"),
        }
    }
}

/// render calc 执行上下文。读 render 字段 + 时钟；写仅限自己实例的 render 字段。
pub struct RenderCtx<'a> {
    pub(crate) store: &'a RenderStore,
    pub(crate) self_id: InstanceId,
    pub(crate) clock: RenderClock,
    /// 本次运行写集（render 命名空间），runtime 收集后做写折叠。
    pub(crate) writes: Vec<(RFieldId, Value)>,
}

impl<'a> RenderCtx<'a> {
    pub fn self_id(&self) -> InstanceId {
        self.self_id
    }

    /// 本 render 帧经过秒数（动态帧率：按时间积分的视觉量用它）。
    pub fn dt(&self) -> f64 {
        self.clock.dt
    }

    /// 插值因子 ∈ [0,1]。
    pub fn alpha(&self) -> f64 {
        self.clock.alpha
    }

    pub fn render_frame(&self) -> u64 {
        self.clock.frame
    }

    /// 读自己实例的 render 字段。
    pub fn read(&self, f: RFieldId) -> Value {
        self.store.read_render(self.self_id, f)
    }

    /// 读任意实例的 render 字段（是读，不是依赖；想被触发须用谓词订阅）。
    pub fn read_of(&self, inst: InstanceId, f: RFieldId) -> Value {
        self.store.read_render(inst, f)
    }

    /// 写自己实例的 render 字段。同字段多次赋值由 runtime 折叠为一条。
    pub fn write(&mut self, f: RFieldId, v: impl Into<Value>) {
        self.writes.push((f, v.into()));
    }
}

/// render 事件反应：挂在 sim 写谓词之后。签名 (ctx, input)。
pub type ReactionFn = Box<dyn Fn(&mut RenderCtx, &RenderInput) + Send + Sync>;

/// render 连续更新：render clock 每帧对每个存活实例运行一次。签名 (ctx)。
pub type ContinuousFn = Box<dyn Fn(&mut RenderCtx) + Send + Sync>;
