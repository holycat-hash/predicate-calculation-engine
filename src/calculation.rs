//! calculation 层：挂在 entity 类型下、predicate 之后（§1.3）。
//!
//! - 输入是前置 predicate 的交付（值的快照，不是引用）
//! - 输出是对**自己实例字段**的 write（写局部性：跨实体影响只能经由数据流）
//! - 快照读：读任何字段读到的都是上一帧已提交的值
//! - 内部默认是任意图灵完备代码；[`crate::runtime::Tier::Kernel`] 档必须提供
//!   [`KernelIr`]，走可机检的数据流子集（C1/D4）

use crate::entity::{EntityTypeId, FIELD_ALIVE, FieldId, InstanceId};
use crate::predicate::CmpOp;
use crate::runtime::{Detect, Residency};
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

/// Kernel 子集的 calculation IR：一个逐实例、无 ambient 副作用的数据流程序。
///
/// 它是执行层的 `behavior -> data` 形态：每个输出字段由一段后缀程序计算，
/// 解释器只读快照 / 输入交付并产出 own 字段写入。无法表达 spawn、destroy、
/// 外部 I/O、任意循环或跳转，因此 IR 子集的 D4 副作用封闭可被注册期机检。
#[derive(Debug, Clone, PartialEq)]
pub struct KernelIr {
    writes: Vec<KernelWrite>,
}

impl KernelIr {
    pub fn new(writes: Vec<KernelWrite>) -> Self {
        KernelIr { writes }
    }

    pub fn writes(&self) -> &[KernelWrite] {
        &self.writes
    }

    pub(crate) fn validate(
        &self,
        calc_name: &str,
        declared_writes: &[FieldId],
        reads: Option<&[FieldId]>,
        input_shape: KernelInputShape,
    ) -> Result<(), String> {
        let mut seen_writes = Vec::new();
        for w in &self.writes {
            if w.field == FIELD_ALIVE {
                return Err(format!(
                    "Kernel IR calculation {calc_name} 不能写 _alive；kernel 子集不含生命周期操作"
                ));
            }
            if seen_writes.contains(&w.field) {
                return Err(format!(
                    "Kernel IR calculation {calc_name} 多次写同一输出字段（IR 输出必须唯一）"
                ));
            }
            seen_writes.push(w.field);
            if !declared_writes.contains(&w.field) {
                return Err(format!(
                    "Kernel IR calculation {calc_name} 写了未声明字段（D1 要求静态写集）"
                ));
            }
            validate_kernel_ops(calc_name, &w.ops, reads, input_shape)?;
        }
        Ok(())
    }

    pub(crate) fn read_fields(&self) -> impl Iterator<Item = FieldId> + '_ {
        self.writes
            .iter()
            .flat_map(|w| w.ops.iter())
            .filter_map(|op| match op {
                KernelOp::ReadOwn(f) => Some(*f),
                _ => None,
            })
    }

    pub(crate) fn run(
        &self,
        snapshot: &dyn SnapshotRead,
        self_id: InstanceId,
        input: &Input,
    ) -> Vec<(FieldId, Value)> {
        let mut stack = Vec::new();
        self.writes
            .iter()
            .map(|w| {
                let v = eval_kernel_ops(&w.ops, snapshot, self_id, input, &mut stack);
                (w.field, v)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum KernelInputShape {
    Each { arity: usize },
    Batch,
    Fold,
}

/// Kernel backend 看到的一根列。
///
/// 这是执行层的 SoA 缓冲形态：默认 backend 会从 runtime 的类型化列中按 lane
/// 读取 `Bool` / `Int` / `Float` / `Vec3` / `Quat`，表达式求值再产生输出列。
/// 异构值或无法去装箱的值退回 `Boxed`，语义保持优先。
#[derive(Debug, Clone, PartialEq)]
pub enum KernelColumn {
    Bool(Vec<bool>),
    Int(Vec<i64>),
    Float(Vec<f64>),
    Vec3(Vec<[f64; 3]>),
    Quat(Vec<[f64; 4]>),
    Boxed(Vec<Value>),
}

impl KernelColumn {
    pub fn len(&self) -> usize {
        match self {
            KernelColumn::Bool(v) => v.len(),
            KernelColumn::Int(v) => v.len(),
            KernelColumn::Float(v) => v.len(),
            KernelColumn::Vec3(v) => v.len(),
            KernelColumn::Quat(v) => v.len(),
            KernelColumn::Boxed(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, lane: usize) -> Value {
        match self {
            KernelColumn::Bool(v) => v.get(lane).map_or(Value::Null, |&v| Value::Bool(v)),
            KernelColumn::Int(v) => v.get(lane).map_or(Value::Null, |&v| Value::Int(v)),
            KernelColumn::Float(v) => v.get(lane).map_or(Value::Null, |&v| Value::Float(v)),
            KernelColumn::Vec3(v) => v.get(lane).map_or(Value::Null, |&v| Value::Vec3(v)),
            KernelColumn::Quat(v) => v.get(lane).map_or(Value::Null, |&v| Value::Quat(v)),
            KernelColumn::Boxed(v) => v.get(lane).cloned().unwrap_or(Value::Null),
        }
    }

    pub fn splat(value: Value, lanes: usize) -> Self {
        match value {
            Value::Bool(v) => KernelColumn::Bool(vec![v; lanes]),
            Value::Int(v) => KernelColumn::Int(vec![v; lanes]),
            Value::Float(v) => KernelColumn::Float(vec![v; lanes]),
            Value::Vec3(v) => KernelColumn::Vec3(vec![v; lanes]),
            Value::Quat(v) => KernelColumn::Quat(vec![v; lanes]),
            v => KernelColumn::Boxed(vec![v; lanes]),
        }
    }

    pub fn from_values(values: Vec<Value>) -> Self {
        if values.iter().all(|v| matches!(v, Value::Bool(_))) {
            return KernelColumn::Bool(
                values
                    .into_iter()
                    .map(|v| match v {
                        Value::Bool(v) => v,
                        _ => unreachable!(),
                    })
                    .collect(),
            );
        }
        if values.iter().all(|v| matches!(v, Value::Int(_))) {
            return KernelColumn::Int(
                values
                    .into_iter()
                    .map(|v| match v {
                        Value::Int(v) => v,
                        _ => unreachable!(),
                    })
                    .collect(),
            );
        }
        if values.iter().all(|v| matches!(v, Value::Float(_))) {
            return KernelColumn::Float(
                values
                    .into_iter()
                    .map(|v| match v {
                        Value::Float(v) => v,
                        _ => unreachable!(),
                    })
                    .collect(),
            );
        }
        if values.iter().all(|v| matches!(v, Value::Vec3(_))) {
            return KernelColumn::Vec3(
                values
                    .into_iter()
                    .map(|v| match v {
                        Value::Vec3(v) => v,
                        _ => unreachable!(),
                    })
                    .collect(),
            );
        }
        if values.iter().all(|v| matches!(v, Value::Quat(_))) {
            return KernelColumn::Quat(
                values
                    .into_iter()
                    .map(|v| match v {
                        Value::Quat(v) => v,
                        _ => unreachable!(),
                    })
                    .collect(),
            );
        }
        KernelColumn::Boxed(values)
    }

    fn f64_at(&self, lane: usize) -> Option<f64> {
        match self {
            KernelColumn::Bool(v) => v.get(lane).map(|&v| if v { 1.0 } else { 0.0 }),
            KernelColumn::Int(v) => v.get(lane).map(|&v| v as f64),
            KernelColumn::Float(v) => v.get(lane).copied(),
            _ => self.get(lane).as_f64(),
        }
    }

    fn bool_at(&self, lane: usize) -> bool {
        match self {
            KernelColumn::Bool(v) => v.get(lane).copied().unwrap_or(false),
            _ => matches!(self.get(lane), Value::Bool(true)),
        }
    }
}

/// Snapshot source for kernel SoA reads.
///
/// Runtime's [`crate::runtime::Store`] implements this by slicing its typed columns by the
/// requested lanes. The default method preserves correctness for other snapshot readers.
pub trait KernelColumnSource: SnapshotRead {
    fn read_own_column(&self, lanes: &[InstanceId], field: FieldId) -> KernelColumn {
        KernelColumn::from_values(lanes.iter().map(|&lane| self.read(lane, field)).collect())
    }
}

/// One backend dispatch batch: same calc, same IR, N lanes.
pub struct KernelBatch<'a> {
    source: &'a dyn KernelColumnSource,
    lanes: &'a [InstanceId],
    inputs: &'a [&'a Input],
    residency: Residency,
}

impl<'a> KernelBatch<'a> {
    pub(crate) fn new(
        source: &'a dyn KernelColumnSource,
        lanes: &'a [InstanceId],
        inputs: &'a [&'a Input],
        residency: Residency,
    ) -> Self {
        KernelBatch {
            source,
            lanes,
            inputs,
            residency,
        }
    }

    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    pub fn lanes(&self) -> &[InstanceId] {
        self.lanes
    }

    pub fn inputs(&self) -> &[&'a Input] {
        self.inputs
    }

    /// Resolved C3 residency for this dispatch group. Backends may use it to choose CPU,
    /// SIMD, or GPU execution and may fall back if unsupported.
    pub fn residency(&self) -> Residency {
        self.residency
    }

    pub fn read_own(&self, field: FieldId) -> KernelColumn {
        self.source.read_own_column(self.lanes, field)
    }

    pub fn input_arg(&self, index: usize) -> KernelColumn {
        KernelColumn::from_values(
            self.inputs
                .iter()
                .map(|input| match input {
                    Input::Each(args) => args.get(index).cloned().unwrap_or(Value::Null),
                    _ => Value::Null,
                })
                .collect(),
        )
    }

    pub fn fold_input(&self) -> KernelColumn {
        KernelColumn::from_values(
            self.inputs
                .iter()
                .map(|input| match input {
                    Input::Fold(v) => v.clone(),
                    _ => Value::Null,
                })
                .collect(),
        )
    }

    pub fn batch_len(&self) -> KernelColumn {
        KernelColumn::Int(
            self.inputs
                .iter()
                .map(|input| match input {
                    Input::Batch(rows) => rows.len() as i64,
                    _ => 0,
                })
                .collect(),
        )
    }
}

/// One output column produced by a [`KernelBackend`].
#[derive(Debug, Clone, PartialEq)]
pub struct KernelColumnWrite {
    pub field: FieldId,
    pub values: KernelColumn,
}

impl KernelColumnWrite {
    pub fn new(field: FieldId, values: KernelColumn) -> Self {
        KernelColumnWrite { field, values }
    }
}

/// Output columns for a kernel batch.
#[derive(Debug, Clone, PartialEq)]
pub struct KernelBatchOutput {
    writes: Vec<KernelColumnWrite>,
}

impl KernelBatchOutput {
    pub fn new(writes: Vec<KernelColumnWrite>) -> Self {
        KernelBatchOutput { writes }
    }

    pub fn writes(&self) -> &[KernelColumnWrite] {
        &self.writes
    }

    pub(crate) fn lane_writes(&self, lane: usize) -> Vec<(FieldId, Value)> {
        self.writes
            .iter()
            .map(|w| (w.field, w.values.get(lane)))
            .collect()
    }
}

/// Pluggable execution backend for kernel IR.
///
/// Core ships [`ScalarKernelBackend`], a SoA column interpreter. SIMD/GPU implementations can
/// implement this trait, advertise supported [`Residency`] hints, and be registered on
/// [`crate::runtime::Runtime`].
pub trait KernelBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn supports_residency(&self, _residency: Residency) -> bool {
        true
    }

    fn run(&self, ir: &KernelIr, batch: KernelBatch<'_>) -> KernelBatchOutput;
}

/// Default backend: scalar interpretation over SoA input/output columns.
#[derive(Debug, Default)]
pub struct ScalarKernelBackend;

impl KernelBackend for ScalarKernelBackend {
    fn name(&self) -> &'static str {
        "scalar-soa"
    }

    fn run(&self, ir: &KernelIr, batch: KernelBatch<'_>) -> KernelBatchOutput {
        let mut stack = Vec::new();
        let writes = ir
            .writes
            .iter()
            .map(|w| {
                KernelColumnWrite::new(w.field, eval_kernel_ops_column(&w.ops, &batch, &mut stack))
            })
            .collect();
        KernelBatchOutput::new(writes)
    }
}

/// 一个 kernel 输出写：`field = eval(ops)`。
#[derive(Debug, Clone, PartialEq)]
pub struct KernelWrite {
    pub field: FieldId,
    pub ops: Vec<KernelOp>,
}

impl KernelWrite {
    pub fn new(field: FieldId, ops: Vec<KernelOp>) -> Self {
        KernelWrite { field, ops }
    }
}

/// Kernel 后缀程序指令。
///
/// 栈约定：
/// - 值源指令压入一个 [`Value`]。
/// - 算术 / 比较 / 逻辑指令弹出操作数并压回结果。
/// - `Select` 按 `cond then else Select` 的顺序编码：弹出 else、then、cond。
#[derive(Debug, Clone, PartialEq)]
pub enum KernelOp {
    Const(Value),
    ReadOwn(FieldId),
    InputArg(usize),
    FoldInput,
    BatchLen,
    Add,
    Sub,
    Mul,
    Div,
    Cmp(CmpOp),
    Not,
    And,
    Or,
    Select,
}

fn validate_kernel_ops(
    calc_name: &str,
    ops: &[KernelOp],
    reads: Option<&[FieldId]>,
    input_shape: KernelInputShape,
) -> Result<(), String> {
    let mut depth = 0usize;
    for op in ops {
        match op {
            KernelOp::Const(_) => {
                depth += 1;
            }
            KernelOp::InputArg(i) => match input_shape {
                KernelInputShape::Each { arity } if *i < arity => {
                    depth += 1;
                }
                KernelInputShape::Each { arity } => {
                    return Err(format!(
                        "Kernel IR calculation {calc_name} 引用了不存在的 each 输入 arg {i}（arity = {arity}）"
                    ));
                }
                _ => {
                    return Err(format!(
                        "Kernel IR calculation {calc_name} 只有 each 交付可使用 InputArg"
                    ));
                }
            },
            KernelOp::FoldInput => match input_shape {
                KernelInputShape::Fold => depth += 1,
                _ => {
                    return Err(format!(
                        "Kernel IR calculation {calc_name} 只有 fold 交付可使用 FoldInput"
                    ));
                }
            },
            KernelOp::BatchLen => match input_shape {
                KernelInputShape::Batch => depth += 1,
                _ => {
                    return Err(format!(
                        "Kernel IR calculation {calc_name} 只有 batch 交付可使用 BatchLen"
                    ));
                }
            },
            KernelOp::ReadOwn(f) => {
                if let Some(reads) = reads
                    && !reads.contains(f)
                {
                    return Err(format!(
                        "Kernel IR calculation {calc_name} 读了未声明字段（C2 读集声明与 IR 不符）"
                    ));
                }
                depth += 1;
            }
            KernelOp::Not => {
                require_kernel_stack(calc_name, op, depth, 1)?;
            }
            KernelOp::Add
            | KernelOp::Sub
            | KernelOp::Mul
            | KernelOp::Div
            | KernelOp::Cmp(_)
            | KernelOp::And
            | KernelOp::Or => {
                require_kernel_stack(calc_name, op, depth, 2)?;
                depth -= 1;
            }
            KernelOp::Select => {
                require_kernel_stack(calc_name, op, depth, 3)?;
                depth -= 2;
            }
        }
    }
    if depth != 1 {
        return Err(format!(
            "Kernel IR calculation {calc_name} 的每个输出表达式必须留下唯一栈顶值"
        ));
    }
    Ok(())
}

fn require_kernel_stack(
    calc_name: &str,
    op: &KernelOp,
    depth: usize,
    needed: usize,
) -> Result<(), String> {
    if depth < needed {
        Err(format!(
            "Kernel IR calculation {calc_name} 在 {op:?} 处栈深不足"
        ))
    } else {
        Ok(())
    }
}

fn eval_kernel_ops(
    ops: &[KernelOp],
    snapshot: &dyn SnapshotRead,
    self_id: InstanceId,
    input: &Input,
    stack: &mut Vec<Value>,
) -> Value {
    stack.clear();
    for op in ops {
        match op {
            KernelOp::Const(v) => stack.push(v.clone()),
            KernelOp::ReadOwn(f) => stack.push(snapshot.read(self_id, *f)),
            KernelOp::InputArg(i) => stack.push(match input {
                Input::Each(args) => args.get(*i).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            }),
            KernelOp::FoldInput => stack.push(match input {
                Input::Fold(v) => v.clone(),
                _ => Value::Null,
            }),
            KernelOp::BatchLen => stack.push(match input {
                Input::Batch(rows) => Value::Int(rows.len() as i64),
                _ => Value::Null,
            }),
            KernelOp::Add => kernel_bin_arith(stack, |x, y| x + y),
            KernelOp::Sub => kernel_bin_arith(stack, |x, y| x - y),
            KernelOp::Mul => kernel_bin_arith(stack, |x, y| x * y),
            KernelOp::Div => kernel_bin_arith(stack, |x, y| x / y),
            KernelOp::Cmp(op) => {
                let r = stack.pop().unwrap_or(Value::Null);
                let l = stack.pop().unwrap_or(Value::Null);
                stack.push(Value::Bool(kernel_cmp_op(&l, *op, &r)));
            }
            KernelOp::Not => {
                let b = kernel_pop_bool(stack);
                stack.push(Value::Bool(!b));
            }
            KernelOp::And => {
                let (b, a) = (kernel_pop_bool(stack), kernel_pop_bool(stack));
                stack.push(Value::Bool(a && b));
            }
            KernelOp::Or => {
                let (b, a) = (kernel_pop_bool(stack), kernel_pop_bool(stack));
                stack.push(Value::Bool(a || b));
            }
            KernelOp::Select => {
                let else_v = stack.pop().unwrap_or(Value::Null);
                let then_v = stack.pop().unwrap_or(Value::Null);
                let cond = kernel_pop_bool(stack);
                stack.push(if cond { then_v } else { else_v });
            }
        }
    }
    stack.pop().unwrap_or(Value::Null)
}

fn eval_kernel_ops_column(
    ops: &[KernelOp],
    batch: &KernelBatch<'_>,
    stack: &mut Vec<KernelColumn>,
) -> KernelColumn {
    stack.clear();
    let lanes = batch.lane_count();
    for op in ops {
        match op {
            KernelOp::Const(v) => stack.push(KernelColumn::splat(v.clone(), lanes)),
            KernelOp::ReadOwn(f) => stack.push(batch.read_own(*f)),
            KernelOp::InputArg(i) => stack.push(batch.input_arg(*i)),
            KernelOp::FoldInput => stack.push(batch.fold_input()),
            KernelOp::BatchLen => stack.push(batch.batch_len()),
            KernelOp::Add => kernel_col_bin_arith(stack, lanes, |x, y| x + y),
            KernelOp::Sub => kernel_col_bin_arith(stack, lanes, |x, y| x - y),
            KernelOp::Mul => kernel_col_bin_arith(stack, lanes, |x, y| x * y),
            KernelOp::Div => kernel_col_bin_arith(stack, lanes, |x, y| x / y),
            KernelOp::Cmp(op) => {
                let r = pop_kernel_col(stack, lanes);
                let l = pop_kernel_col(stack, lanes);
                stack.push(KernelColumn::Bool(
                    (0..lanes)
                        .map(|lane| kernel_cmp_op(&l.get(lane), *op, &r.get(lane)))
                        .collect(),
                ));
            }
            KernelOp::Not => {
                let v = pop_kernel_col(stack, lanes);
                stack.push(KernelColumn::Bool(
                    (0..lanes).map(|lane| !v.bool_at(lane)).collect(),
                ));
            }
            KernelOp::And => {
                let r = pop_kernel_col(stack, lanes);
                let l = pop_kernel_col(stack, lanes);
                stack.push(KernelColumn::Bool(
                    (0..lanes)
                        .map(|lane| l.bool_at(lane) && r.bool_at(lane))
                        .collect(),
                ));
            }
            KernelOp::Or => {
                let r = pop_kernel_col(stack, lanes);
                let l = pop_kernel_col(stack, lanes);
                stack.push(KernelColumn::Bool(
                    (0..lanes)
                        .map(|lane| l.bool_at(lane) || r.bool_at(lane))
                        .collect(),
                ));
            }
            KernelOp::Select => {
                let else_v = pop_kernel_col(stack, lanes);
                let then_v = pop_kernel_col(stack, lanes);
                let cond = pop_kernel_col(stack, lanes);
                stack.push(KernelColumn::from_values(
                    (0..lanes)
                        .map(|lane| {
                            if cond.bool_at(lane) {
                                then_v.get(lane)
                            } else {
                                else_v.get(lane)
                            }
                        })
                        .collect(),
                ));
            }
        }
    }
    pop_kernel_col(stack, lanes)
}

fn pop_kernel_col(stack: &mut Vec<KernelColumn>, lanes: usize) -> KernelColumn {
    stack
        .pop()
        .unwrap_or_else(|| KernelColumn::splat(Value::Null, lanes))
}

fn kernel_col_bin_arith(stack: &mut Vec<KernelColumn>, lanes: usize, f: fn(f64, f64) -> f64) {
    let r = pop_kernel_col(stack, lanes);
    let l = pop_kernel_col(stack, lanes);
    stack.push(KernelColumn::from_values(
        (0..lanes)
            .map(|lane| match (l.f64_at(lane), r.f64_at(lane)) {
                (Some(x), Some(y)) => Value::Float(f(x, y)),
                _ => Value::Null,
            })
            .collect(),
    ));
}

fn kernel_pop_bool(stack: &mut Vec<Value>) -> bool {
    matches!(stack.pop(), Some(Value::Bool(true)))
}

fn kernel_bin_arith(stack: &mut Vec<Value>, f: fn(f64, f64) -> f64) {
    let r = stack.pop().unwrap_or(Value::Null);
    let l = stack.pop().unwrap_or(Value::Null);
    match (l.as_f64(), r.as_f64()) {
        (Some(x), Some(y)) => stack.push(Value::Float(f(x, y))),
        _ => stack.push(Value::Null),
    }
}

fn kernel_cmp_op(l: &Value, op: CmpOp, r: &Value) -> bool {
    match op {
        CmpOp::Eq => kernel_val_eq(l, r),
        CmpOp::Ne => !kernel_val_eq(l, r),
        _ => match l.cmp_num(r) {
            Some(o) => match op {
                CmpOp::Lt => o.is_lt(),
                CmpOp::Le => o.is_le(),
                CmpOp::Gt => o.is_gt(),
                CmpOp::Ge => o.is_ge(),
                _ => unreachable!(),
            },
            None => false,
        },
    }
}

fn kernel_val_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
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
        if self.detect != Detect::Silent
            && let Some(reads) = self.reads
            && !reads.contains(&field)
        {
            let msg = format!(
                "[PCE] calculation {} 读了未声明字段（C2 读集声明与实际读取不符）",
                self.calc_name
            );
            if self.detect == Detect::Strict {
                panic!("{msg}");
            }
            eprintln!("{msg}");
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
        if field == FIELD_ALIVE {
            panic!("_alive 是 runtime 生命周期位；calculation 请使用 destroy_self()");
        }
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
        self.writes.push((FIELD_ALIVE, Value::Bool(false)));
    }
}

/// calculation 本体：任意图灵完备代码，签名固定为 (ctx, input)。
/// Send + Sync：执行阶段零序约束下可任意并行调度（D1 + 写局部性保证无竞争）。
pub type CalcFn = Box<dyn Fn(&mut Ctx, &Input) + Send + Sync>;
