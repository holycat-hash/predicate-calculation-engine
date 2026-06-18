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
use super::local::{LocalStore, RenderLocalCommand, RenderLocalId, RenderLocalTypeId};
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
    /// 本次运行请求创建的 render-local 临时实体。命令在 calc 结束后由 render runtime
    /// 统一提交，避免 calc 持有本地池的可变借用。
    pub(crate) local_commands: Vec<RenderLocalCommand>,
}

impl<'a> RenderCtx<'a> {
    pub fn self_id(&self) -> InstanceId {
        self.self_id
    }

    /// 本 render 帧经过秒数（动态帧率：按时间积分的视觉量用它）。
    ///
    /// 范式（DF5）：线性进度用 `x += rate * dt()`；**指数缓动 / 相机阻尼须用
    /// `x += (target − x) * (1 − exp(−k·dt()))`**，不要用定值 lerp 因子
    /// `x += (target − x) * 0.1`——后者收敛速度随帧率漂（帧多过冲、帧少迟滞）。
    /// 前者每帧把残差乘 `exp(−k·dt)`，N 帧累积恰为 `exp(−k·Σdt)`，与子分无关。
    pub fn dt(&self) -> f64 {
        self.clock.dt
    }

    /// 插值因子 ∈ `[0,1]`。
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
    /// 在 continuous 扫描中遵循**快照读**（与 sim 对齐，B3）：读到的是本帧连续写提交前的
    /// 值，故跨实例看到上一帧的连续输出——结果与扫描序 / 并行线程数无关。
    pub fn read_of(&self, inst: InstanceId, f: RFieldId) -> Value {
        self.store.read_render(inst, f)
    }

    /// 写自己实例的 render 字段。同字段多次赋值由 runtime 折叠为一条。
    pub fn write(&mut self, f: RFieldId, v: impl Into<Value>) {
        self.writes.push((f, v.into()));
    }

    /// 创建一个 render-local 临时实体（粒子 / 飘字等）。它不会进入 sim，也不会出现在
    /// shared entity 的生命周期里；render runtime 在本帧 calc 结束后把命令提交到本地池。
    pub fn spawn_local(&mut self, ty: RenderLocalTypeId, init: Vec<(RFieldId, Value)>) {
        self.local_commands
            .push(RenderLocalCommand::Spawn { ty, init });
    }
}

/// render-local calc 执行上下文。读写 render-local 字段；可创建更多 local 实体，也可
/// 销毁自身。用于粒子 / 飘字这类 render 自管寿命的临时实体通道。
pub struct RenderLocalCtx<'a> {
    pub(crate) store: &'a LocalStore,
    pub(crate) self_id: RenderLocalId,
    pub(crate) clock: RenderClock,
    pub(crate) writes: Vec<(RFieldId, Value)>,
    pub(crate) local_commands: Vec<RenderLocalCommand>,
}

impl<'a> RenderLocalCtx<'a> {
    pub fn self_id(&self) -> RenderLocalId {
        self.self_id
    }

    pub fn dt(&self) -> f64 {
        self.clock.dt
    }

    pub fn alpha(&self) -> f64 {
        self.clock.alpha
    }

    pub fn render_frame(&self) -> u64 {
        self.clock.frame
    }

    pub fn read(&self, f: RFieldId) -> Value {
        self.store.read(self.self_id, f)
    }

    pub fn read_of(&self, id: RenderLocalId, f: RFieldId) -> Value {
        self.store.read(id, f)
    }

    pub fn write(&mut self, f: RFieldId, v: impl Into<Value>) {
        self.writes.push((f, v.into()));
    }

    pub fn spawn_local(&mut self, ty: RenderLocalTypeId, init: Vec<(RFieldId, Value)>) {
        self.local_commands
            .push(RenderLocalCommand::Spawn { ty, init });
    }

    /// 结束自身生命周期。销毁在本次 local calc 批次结束后提交，所以同帧读写仍保持
    /// 快照语义；提交视图会跳过已销毁实体。
    pub fn destroy_self(&mut self) {
        self.local_commands
            .push(RenderLocalCommand::Destroy(self.self_id));
    }
}

/// render 事件反应：挂在 sim 写谓词之后。签名 (ctx, input)。
pub type ReactionFn = Box<dyn Fn(&mut RenderCtx, &RenderInput) + Send + Sync>;

/// render 连续更新：render clock 每帧对每个在场实例运行一次。签名 (ctx)。
pub type ContinuousFn = Box<dyn Fn(&mut RenderCtx) + Send + Sync>;

/// render-local 连续更新：render clock 每帧对每个本地实体运行一次。签名 (ctx)。
pub type LocalContinuousFn = Box<dyn Fn(&mut RenderLocalCtx) + Send + Sync>;
