//! calculation 层：挂在 entity 类型下、predicate 之后（§1.3）。
//!
//! - 输入是前置 predicate 的交付（值的快照，不是引用）
//! - 输出是对**自己实例字段**的 write（写局部性：跨实体影响只能经由数据流）
//! - 快照读：读任何字段读到的都是上一帧已提交的值
//! - 内部是任意图灵完备代码（[`crate::runtime::Tier::Kernel`] 档自愿受限，C1）

use crate::entity::{EntityTypeId, FieldId, InstanceId};
use crate::runtime::Detect;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CalcId(pub u32);

/// 谓词交付给 calculation 的输入。形态由 delivery 的势唯一决定
/// （单谓词制保证输入签名单一，§1.4）。
#[derive(Debug, Clone)]
pub enum Input {
    /// each：一条命中的投影元组。
    Each(Vec<Value>),
    /// batch：整帧命中的多重集（顺序未定义，D3；Canonical 档买回规范序，C4）。
    Batch(Vec<Vec<Value>>),
    /// fold：聚合值。
    Fold(Value),
}

impl Input {
    /// each 交付的投影元组。其他势 panic（注册期已定形态，不该走到）。
    pub fn args(&self) -> &[Value] {
        match self {
            Input::Each(v) => v,
            _ => panic!("Input::args 仅适用于 each 交付"),
        }
    }

    /// each 投影元组的第 i 项。
    pub fn arg(&self, i: usize) -> &Value {
        &self.args()[i]
    }

    /// batch 交付的多重集（D3：消费逻辑必须顺序无关）。
    pub fn rows(&self) -> &[Vec<Value>] {
        match self {
            Input::Batch(v) => v,
            _ => panic!("Input::rows 仅适用于 batch 交付"),
        }
    }

    /// fold 交付的聚合值。
    pub fn agg(&self) -> &Value {
        match self {
            Input::Fold(v) => v,
            _ => panic!("Input::agg 仅适用于 fold 交付"),
        }
    }
}

/// calculation 的执行上下文。
///
/// 读取走快照（上一帧提交值），写入仅允许自己实例的字段——
/// 写局部性由本接口静态保证：`write` 不接受实例参数。
pub struct Ctx<'rt> {
    pub(crate) snapshot: &'rt dyn SnapshotRead,
    pub(crate) self_id: InstanceId,
    /// 本次运行的写集（线程本地缓冲），runtime 收集后做写折叠（§2）。
    pub(crate) writes: Vec<(FieldId, Value)>,
    /// 创建请求，帧边界生效（runtime 代写 `_alive = true`，§6.3）。
    pub(crate) spawns: Vec<(EntityTypeId, Vec<(FieldId, Value)>)>,
    /// C5 检测档位。
    pub(crate) detect: Detect,
    /// C2 声明读集（None = 未声明，退化为 profile 猜测）。
    pub(crate) reads: Option<&'rt [FieldId]>,
    /// C1 kernel 档（禁 spawn 等动态分配类操作）。
    pub(crate) kernel: bool,
    pub(crate) calc_name: &'rt str,
}

/// runtime 提供的快照读视图（帧 N 已提交值；本帧写入不可见）。
pub trait SnapshotRead: Sync {
    fn read(&self, inst: InstanceId, field: FieldId) -> Value;
}

impl<'rt> Ctx<'rt> {
    pub fn self_id(&self) -> InstanceId {
        self.self_id
    }

    /// 读自己实例的字段（上一帧快照）。
    /// 声明过读集（C2）且非 Silent 档时检测越界读。
    pub fn read_own(&self, field: FieldId) -> Value {
        if self.detect != Detect::Silent {
            if let Some(reads) = self.reads {
                if !reads.contains(&field) {
                    let msg = format!(
                        "[PCE] calculation {} 读了未声明字段（C2 读集声明与实际读取不符）",
                        self.calc_name
                    );
                    if self.detect == Detect::Strict {
                        panic!("{msg}");
                    }
                    eprintln!("{msg}");
                }
            }
        }
        self.snapshot.read(self.self_id, field)
    }

    /// 读任意实例的字段（上一帧快照）。
    /// 注意：这是读，不是依赖——想被对方的变化**触发**，必须用 predicate 订阅。
    pub fn read(&self, inst: InstanceId, field: FieldId) -> Value {
        self.snapshot.read(inst, field)
    }

    /// 写自己实例的字段。同一字段多次赋值由 runtime 折叠为一条写记录：
    /// new 取最终值，old 取上一帧提交值（§2 写折叠）。
    pub fn write(&mut self, field: FieldId, value: impl Into<Value>) {
        self.writes.push((field, value.into()));
    }

    /// 请求创建实例（帧边界生效）。观察者用 `type(E, _alive) where became(true)` 感知出生。
    /// Kernel 档（C1）禁用：kernel 子集无动态分配。
    pub fn spawn(&mut self, ty: EntityTypeId, init: Vec<(FieldId, Value)>) {
        if self.kernel && self.detect != Detect::Silent {
            let msg = format!(
                "[PCE] calculation {} 标注为 Kernel 档（C1）却调用 spawn（动态分配）",
                self.calc_name
            );
            if self.detect == Detect::Strict {
                panic!("{msg}");
            }
            eprintln!("{msg}");
        }
        self.spawns.push((ty, init));
    }

    /// 自决（销毁的唯一入口，§6.3）。等价于 `write(_alive, false)`。
    /// 「杀死他人」必须经由数据流请求（§7 示例 2）。
    pub fn destroy_self(&mut self) {
        self.writes.push((crate::entity::FIELD_ALIVE, Value::Bool(false)));
    }
}

/// calculation 本体：任意图灵完备代码，签名固定为 (ctx, input)。
/// Send + Sync：执行阶段零序约束下可任意并行调度（D1 + 写局部性保证无竞争）。
pub type CalcFn = Box<dyn Fn(&mut Ctx, &Input) + Send + Sync>;
