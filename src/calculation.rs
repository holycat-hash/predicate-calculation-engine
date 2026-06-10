//! calculation 层：挂在 entity 类型下、predicate 之后（§1.3）。
//!
//! - 输入是前置 predicate 的交付（值的快照，不是引用）
//! - 输出是对**自己实例字段**的 write（写局部性：跨实例影响只能经由数据流）
//! - 快照读：读任何字段读到的都是上一帧已提交的值
//! - 内部是任意图灵完备代码

use crate::entity::{EntityTypeId, FieldId, InstanceId};
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CalcId(pub u32);

/// 谓词交付给 calculation 的输入。形态由 delivery 的势唯一决定
/// （单谓词制保证输入签名单一，§1.4）。
#[derive(Debug, Clone)]
pub enum Input {
    /// each：一条命中的投影元组。
    Each(Vec<Value>),
    /// batch：整帧命中的多重集（顺序未定义，D3）。
    Batch(Vec<Vec<Value>>),
    /// fold：聚合值。
    Fold(Value),
}

/// calculation 的执行上下文。
///
/// 读取走快照（上一帧提交值），写入仅允许自己实例的字段——
/// 写局部性由本接口静态保证：`write` 不接受实例参数。
pub struct Ctx<'rt> {
    pub(crate) snapshot: &'rt dyn SnapshotRead,
    pub(crate) self_id: InstanceId,
    /// 本次运行的写集，runtime 收集后做写折叠（§2）。
    pub(crate) writes: Vec<(FieldId, Value)>,
    /// 创建请求，帧边界生效（runtime 代写 `_alive = true`，§6.3）。
    pub(crate) spawns: Vec<(EntityTypeId, Vec<(FieldId, Value)>)>,
}

/// runtime 提供的快照读视图（帧 N 已提交值；本帧写入不可见）。
pub trait SnapshotRead {
    fn read(&self, inst: InstanceId, field: FieldId) -> Value;
}

impl<'rt> Ctx<'rt> {
    pub fn self_id(&self) -> InstanceId {
        self.self_id
    }

    /// 读自己实例的字段（上一帧快照）。
    pub fn read_own(&self, field: FieldId) -> Value {
        self.snapshot.read(self.self_id, field)
    }

    /// 读任意实例的字段（上一帧快照）。
    /// 注意：这是读，不是依赖——想被对方的变化**触发**，必须用 predicate 订阅。
    pub fn read(&self, inst: InstanceId, field: FieldId) -> Value {
        self.snapshot.read(inst, field)
    }

    /// 写自己实例的字段。同一字段多次赋值由 runtime 折叠为一条写记录：
    /// new 取最终值，old 取上一帧提交值（§2 写折叠）。
    pub fn write(&mut self, field: FieldId, value: Value) {
        self.writes.push((field, value));
    }

    /// 请求创建实例（帧边界生效）。观察者用 `type(E, _alive) where became(true)` 感知出生。
    pub fn spawn(&mut self, ty: EntityTypeId, init: Vec<(FieldId, Value)>) {
        self.spawns.push((ty, init));
    }

    /// 自决（销毁的唯一入口，§6.3）。等价于 `write(_alive, false)`。
    /// 「杀死他人」必须经由数据流请求（§7 示例 2）。
    pub fn destroy_self(&mut self) {
        self.writes.push((crate::entity::FIELD_ALIVE, Value::Bool(false)));
    }
}

/// calculation 本体：任意图灵完备代码，签名固定为 (ctx, input)。
pub type CalcFn = Box<dyn Fn(&mut Ctx, &Input)>;
