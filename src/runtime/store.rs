//! SoA 列存储（白送优化 A「SoA 列存」）：(type, field) → 稠密列。
//!
//! 买单约束：schema 注册期定型 + 交付是值快照 + ref 是 id 非指针——
//! 没有任何指针逃逸到用户侧，runtime 拥有布局主权。cell 本来就是列单元。
//!
//! ## 类型化去装箱列（白送优化）
//! 列不再是 `Vec<Value>`（每格一个 24+ 字节装箱枚举），而是按字段 schema 默认值
//! 定型的[类型化无装箱列][`Column`]：`Bool→Vec<bool>`、`Int→Vec<i64>`、
//! `Float→Vec<f64>`，其余（Str/Ref/Map/Null）落 `Boxed`。收益无条件正（白送）：
//! 列扫描密度 ×3~×8、无逐格堆指针、快照即连续 memcpy（见 [`super::Snapshot`]）。
//! 诚实条款：若某格写入与列类型不符（如向 Int 列写 Float/Null），该列**去优化**
//! 为 `Boxed`（一次性、保精确往返），正确性永不受损。
//!
//! 行身份策略（C6，开发者每 type 二选一）：
//! - [`RowPolicy::Stable`]：零间接重映射、死亡留洞（洞可复用），行号终生不变；
//! - [`RowPolicy::Compact`]：行恒稠密，死亡时 swap-remove 重映射，遍历最快。
//! 鱼与熊掌结构性不可兼得，按 churn 特征选。
//!
//! 诚实条款：id 复用 + 代际号意味着每次访问至少一次间接（id→row）与一次
//! 代际比较——这是「无指针」的固定税，两种策略都付。

use crate::entity::{EntityTypeId, FIELD_ALIVE, FieldDef, FieldId, InstanceId};
use crate::value::Value;

/// 类型化无装箱列。列类型由字段 schema 默认值在注册期定型；类型不符的写入
/// 触发一次性[去优化][`Column::boxify`]到 `Boxed`（保精确往返）。
#[derive(Debug, Clone)]
pub(crate) enum Column {
    Bool(Vec<bool>),
    Int(Vec<i64>),
    Float(Vec<f64>),
    /// 三维向量列（平移 / 缩放 / 轴）。内联 `[f64;3]` SoA——transform 是 render
    /// 最热、最频扫的数据，去装箱密度收益在此最大。
    Vec3(Vec<[f64; 3]>),
    /// 四元数列（方向）。内联 `[f64;4]` SoA。
    Quat(Vec<[f64; 4]>),
    /// 装箱回退：异构 / Str / Ref / Map / Null 类型字段。
    Boxed(Vec<Value>),
}

impl Column {
    /// 按字段默认值定型并填充 `n` 行。Bool/Int/Float 走无装箱列，其余落 Boxed。
    fn with_default(default: &Value, n: usize) -> Column {
        match default {
            Value::Bool(b) => Column::Bool(vec![*b; n]),
            Value::Int(i) => Column::Int(vec![*i; n]),
            Value::Float(f) => Column::Float(vec![*f; n]),
            Value::Vec3(a) => Column::Vec3(vec![*a; n]),
            Value::Quat(a) => Column::Quat(vec![*a; n]),
            other => Column::Boxed(vec![other.clone(); n]),
        }
    }

    /// 读一格（重建 [`Value`]）。OOB → Null（防御，正常路径行号已由 row_of 校验）。
    fn get(&self, row: usize) -> Value {
        match self {
            Column::Bool(c) => c.get(row).map_or(Value::Null, |&b| Value::Bool(b)),
            Column::Int(c) => c.get(row).map_or(Value::Null, |&i| Value::Int(i)),
            Column::Float(c) => c.get(row).map_or(Value::Null, |&f| Value::Float(f)),
            Column::Vec3(c) => c.get(row).map_or(Value::Null, |&a| Value::Vec3(a)),
            Column::Quat(c) => c.get(row).map_or(Value::Null, |&a| Value::Quat(a)),
            Column::Boxed(c) => c.get(row).cloned().unwrap_or(Value::Null),
        }
    }

    /// 写一格。值与列类型不符 → 去优化为 Boxed 再写（一次性，正确性优先）。
    fn set(&mut self, row: usize, v: Value) {
        match (&mut *self, &v) {
            (Column::Bool(c), Value::Bool(b)) => c[row] = *b,
            (Column::Int(c), Value::Int(i)) => c[row] = *i,
            (Column::Float(c), Value::Float(f)) => c[row] = *f,
            (Column::Vec3(c), Value::Vec3(a)) => c[row] = *a,
            (Column::Quat(c), Value::Quat(a)) => c[row] = *a,
            (Column::Boxed(c), _) => c[row] = v,
            _ => {
                self.boxify();
                if let Column::Boxed(c) = self {
                    c[row] = v;
                }
            }
        }
    }

    /// 追加一行。
    fn push(&mut self, v: Value) {
        match (&mut *self, &v) {
            (Column::Bool(c), Value::Bool(b)) => c.push(*b),
            (Column::Int(c), Value::Int(i)) => c.push(*i),
            (Column::Float(c), Value::Float(f)) => c.push(*f),
            (Column::Vec3(c), Value::Vec3(a)) => c.push(*a),
            (Column::Quat(c), Value::Quat(a)) => c.push(*a),
            (Column::Boxed(c), _) => c.push(v),
            _ => {
                self.boxify();
                if let Column::Boxed(c) = self {
                    c.push(v);
                }
            }
        }
    }

    fn swap_remove(&mut self, row: usize) {
        match self {
            Column::Bool(c) => {
                c.swap_remove(row);
            }
            Column::Int(c) => {
                c.swap_remove(row);
            }
            Column::Float(c) => {
                c.swap_remove(row);
            }
            Column::Vec3(c) => {
                c.swap_remove(row);
            }
            Column::Quat(c) => {
                c.swap_remove(row);
            }
            Column::Boxed(c) => {
                c.swap_remove(row);
            }
        }
    }

    /// Stable 死亡留洞：仅 Boxed 需要落 Null 以释放堆（Str/Map）；无装箱列的死格
    /// 不可达（row_of 拒绝），无需清。
    fn clear_row(&mut self, row: usize) {
        if let Column::Boxed(c) = self {
            c[row] = Value::Null;
        }
    }

    /// `_alive` / 其他 Bool 列的稠密位扫描入口（白送优化「ECS 快路 / type 扇出」
    /// 顺序列访问）。非 Bool（已去优化）返回 None，调用方回退逐格 get。
    fn as_bool_slice(&self) -> Option<&[bool]> {
        match self {
            Column::Bool(c) => Some(c),
            _ => None,
        }
    }

    /// 去优化：把当前类型化列原样物化为 Boxed（保精确往返）。
    fn boxify(&mut self) {
        let boxed = match self {
            Column::Bool(c) => c.iter().map(|&b| Value::Bool(b)).collect(),
            Column::Int(c) => c.iter().map(|&i| Value::Int(i)).collect(),
            Column::Float(c) => c.iter().map(|&f| Value::Float(f)).collect(),
            Column::Vec3(c) => c.iter().map(|&a| Value::Vec3(a)).collect(),
            Column::Quat(c) => c.iter().map(|&a| Value::Quat(a)).collect(),
            Column::Boxed(_) => return,
        };
        *self = Column::Boxed(boxed);
    }
}

/// 行身份策略（C6）。每 entity 类型注册期二选一，可由遥测（profiler）建议，
/// 但最终由开发者 pin。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowPolicy {
    /// 稳定行：行号终生不变，死亡留洞（洞复用给新实例）。低 churn / 大行适用。
    #[default]
    Stable,
    /// 压缩行：行恒稠密（swap-remove + 重映射 pass，帧界批处理）。
    /// 高频整列遍历（ECS 快路、type scope 扇出）适用。
    Compact,
}

const NO_ROW: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct IdSlot {
    /// 当前（或下一任）住户的代际号。释放时 +1 防 ABA。
    generation: u32,
    /// id → 行号间接（固定税）。NO_ROW = 空置。
    row: u32,
}

/// 单个 entity 类型的列式存储。
#[derive(Clone)]
pub(crate) struct TypeStore {
    pub name: String,
    pub fields: Vec<FieldDef>,
    /// 注册期元数据（单实例类型恒有且只有实例 0，§1.2）。
    #[allow(dead_code)]
    pub singleton: bool,
    pub policy: RowPolicy,
    /// SoA：cols[field] 是类型化无装箱列。同字段连续存放——type scope 扇出 /
    /// ECS 快路是顺序列扫描，路由产物即访问 schedule（「完美预取」的结构前提）。
    cols: Vec<Column>,
    /// row → id / 代际（迭代时反向构造 InstanceId，无需回查 sparse 表）。
    row_id: Vec<u32>,
    row_gen: Vec<u32>,
    /// Stable 策略的洞标记；Compact 下恒 true。
    row_live: Vec<bool>,
    id_slot: Vec<IdSlot>,
    free_ids: Vec<u32>,
    /// Stable 策略可复用的洞。
    free_rows: Vec<u32>,
}

/// 已提交数据快照（双缓冲的「前台」）。
///
/// 白送优化「双缓冲 = 单存储 + 写日志」：W 稀疏是设计公理，`old` 只需写过的
/// cell——恰好就是写日志本身（[`super::WriteRec`]），不需要 2× 全量拷贝。
/// 执行期本结构只读。
///
/// `Clone`：类型化列使整存克隆退化为连续 memcpy——廉价全存快照 / 回滚
/// （[`super::Snapshot`]，GGPO 式 netcode）的物理前提。
#[derive(Clone)]
pub struct Store {
    pub(crate) types: Vec<TypeStore>,
}

impl Store {
    pub(crate) fn new() -> Self {
        Store { types: vec![] }
    }

    pub(crate) fn add_type(
        &mut self,
        name: &str,
        fields: Vec<FieldDef>,
        singleton: bool,
        policy: RowPolicy,
    ) -> EntityTypeId {
        let ty = EntityTypeId(self.types.len() as u32);
        // 列按字段默认值定型（类型化无装箱列）。
        let cols = fields
            .iter()
            .map(|f| Column::with_default(&f.default, 0))
            .collect();
        self.types.push(TypeStore {
            name: name.to_string(),
            fields,
            singleton,
            policy,
            cols,
            row_id: vec![],
            row_gen: vec![],
            row_live: vec![],
            id_slot: vec![],
            free_ids: vec![],
            free_rows: vec![],
        });
        ty
    }

    pub(crate) fn type_name(&self, ty: EntityTypeId) -> &str {
        &self.types[ty.0 as usize].name
    }

    pub(crate) fn fields(&self, ty: EntityTypeId) -> &[FieldDef] {
        &self.types[ty.0 as usize].fields
    }

    pub(crate) fn field_name(&self, ty: EntityTypeId, f: FieldId) -> &str {
        &self.types[ty.0 as usize].fields[f.0 as usize].name
    }

    pub(crate) fn try_field(&self, ty: EntityTypeId, name: &str) -> Result<FieldId, String> {
        let t = &self.types[ty.0 as usize];
        t.fields
            .iter()
            .position(|f| f.name == name)
            .map(|i| FieldId(i as u32))
            .ok_or_else(|| format!("类型 {} 无字段 {name}", t.name))
    }

    pub(crate) fn is_ref_field(&self, ty: EntityTypeId, field: FieldId) -> bool {
        self.types[ty.0 as usize].fields[field.0 as usize].is_ref
    }

    /// id → row 解析：代际不匹配（旧 ref 指向已复用的 id）→ None——ABA 防护。
    #[inline]
    fn row_of(&self, inst: InstanceId) -> Option<(usize, usize)> {
        let t = self.types.get(inst.ty.0 as usize)?;
        let s = t.id_slot.get(inst.id as usize)?;
        (s.generation == inst.generation && s.row != NO_ROW)
            .then_some((inst.ty.0 as usize, s.row as usize))
    }

    /// 快照读。死实例 / 旧代际 ref 读到 Null。
    pub fn read(&self, inst: InstanceId, field: FieldId) -> Value {
        match self.row_of(inst) {
            Some((t, r)) => self.types[t]
                .cols
                .get(field.0 as usize)
                .map_or(Value::Null, |c| c.get(r)),
            None => Value::Null,
        }
    }

    pub fn alive(&self, inst: InstanceId) -> bool {
        matches!(self.read(inst, FIELD_ALIVE), Value::Bool(true))
    }

    /// 稠密遍历该类型全体存活实例。Compact 策略下行恒稠密；
    /// Stable 下跳洞。type scope 扇出与 ECS 快路（白送优化）走这里。
    /// `_alive` 列恒为无装箱 Bool 列——位 slice 顺序扫描（向量化前提）。
    pub(crate) fn for_each_alive(&self, ty: EntityTypeId, mut f: impl FnMut(InstanceId)) {
        let t = &self.types[ty.0 as usize];
        match t.cols[FIELD_ALIVE.0 as usize].as_bool_slice() {
            Some(alive) => {
                for r in 0..t.row_id.len() {
                    if t.row_live[r] && alive[r] {
                        f(InstanceId {
                            ty,
                            id: t.row_id[r],
                            generation: t.row_gen[r],
                        });
                    }
                }
            }
            None => {
                let col = &t.cols[FIELD_ALIVE.0 as usize];
                for r in 0..t.row_id.len() {
                    if t.row_live[r] && matches!(col.get(r), Value::Bool(true)) {
                        f(InstanceId {
                            ty,
                            id: t.row_id[r],
                            generation: t.row_gen[r],
                        });
                    }
                }
            }
        }
    }

    pub fn alive_instances(&self, ty: EntityTypeId) -> Vec<InstanceId> {
        let mut v = vec![];
        self.for_each_alive(ty, |i| v.push(i));
        v
    }

    pub(crate) fn set(&mut self, inst: InstanceId, field: FieldId, v: Value) {
        if let Some((t, r)) = self.row_of(inst) {
            self.types[t].cols[field.0 as usize].set(r, v);
        }
    }

    pub(crate) fn alloc(&mut self, ty: EntityTypeId) -> InstanceId {
        let t = &mut self.types[ty.0 as usize];
        // id 分配：复用归还的 id，代际号已在释放时 +1
        let id = match t.free_ids.pop() {
            Some(id) => id,
            None => {
                t.id_slot.push(IdSlot {
                    generation: 0,
                    row: NO_ROW,
                });
                (t.id_slot.len() - 1) as u32
            }
        };
        let generation = t.id_slot[id as usize].generation;
        // 行分配：Stable 复用洞；Compact 恒追加（行恒稠密）
        let row = match t.policy {
            RowPolicy::Stable => t.free_rows.pop(),
            RowPolicy::Compact => None,
        };
        let row = match row {
            Some(r) => {
                let r = r as usize;
                for (ci, col) in t.cols.iter_mut().enumerate() {
                    col.set(r, t.fields[ci].default.clone());
                }
                t.row_id[r] = id;
                t.row_gen[r] = generation;
                t.row_live[r] = true;
                r as u32
            }
            None => {
                for (ci, col) in t.cols.iter_mut().enumerate() {
                    col.push(t.fields[ci].default.clone());
                }
                t.row_id.push(id);
                t.row_gen.push(generation);
                t.row_live.push(true);
                (t.row_id.len() - 1) as u32
            }
        };
        t.id_slot[id as usize].row = row;
        InstanceId { ty, id, generation }
    }

    /// 释放：id 归还复用、代际号 +1 防 ABA（§6.3）；行按策略处理（C6）。
    /// 调用发生在帧边界结算（settle_death）——压缩重映射因此是帧界批处理
    /// pass，帧内行结构稳定（SIMD/并行安全的前提）。
    pub(crate) fn release(&mut self, inst: InstanceId) {
        let Some((ti, r)) = self.row_of(inst) else {
            return;
        };
        let t = &mut self.types[ti];
        t.id_slot[inst.id as usize] = IdSlot {
            generation: inst.generation + 1,
            row: NO_ROW,
        };
        t.free_ids.push(inst.id);
        match t.policy {
            RowPolicy::Stable => {
                // 死亡留洞：Boxed 列清空释放堆，行号保留待复用（类型化列无堆，免清）
                for col in &mut t.cols {
                    col.clear_row(r);
                }
                t.row_live[r] = false;
                t.free_rows.push(r as u32);
            }
            RowPolicy::Compact => {
                // swap-remove 重映射：末行搬入洞位，修其 id→row 间接
                let last = t.row_id.len() - 1;
                for col in &mut t.cols {
                    col.swap_remove(r);
                }
                t.row_id.swap_remove(r);
                t.row_gen.swap_remove(r);
                t.row_live.swap_remove(r);
                if r != last {
                    let moved_id = t.row_id[r] as usize;
                    t.id_slot[moved_id].row = r as u32;
                }
            }
        }
    }
}
