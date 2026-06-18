//! 共享类型化去装箱列内核（白送优化）：sim 存储与 render sidecar 的**共同底座**。
//!
//! ## 为什么是一个独立内核（seam 在此）
//! 一个列式存储分两层关注点，它们的演化轴正交，故拆成可独立替换的两半：
//! - **类型化列内核（本模块，共享）**：一根 `(field) → 稠密无装箱列` 的存储原语，
//!   按行号（`usize`）寻址，**对生命周期 / 行身份一无所知**。get/set/push/resize/
//!   swap_remove 全部以 row 为唯一坐标。
//! - **行 / 存活内核（每个存储各自一份，可替换）**：把实例 id 映射到行号、管代际防
//!   ABA、管存活与回收。sim 侧是代际 `id_slot` 间接 + [`RowPolicy`] 压缩/留洞
//!   （[`crate::runtime::store`]）；render sidecar 侧是 `row = sim id` 直址 + 位图存活
//!   （[`crate::render::store`]，被动跟随 sim 生灭）。两者天差地别，但都只透过本内核的
//!   row 寻址 API 触碰数据——所以行/存活内核能各管各的、互不污染。
//!
//! 这条 seam 让**去装箱收益**对两侧都生效：render 最热的每帧 transform 插值输出列
//! 不再是装箱的 `Vec<Value>`（每格一个宽枚举 + 判别位 + 稀疏访问），而是与 sim 同款的
//! 无装箱 `Vec<[f64;3]>` / `Vec<[f64;4]>` / `Vec<f64>` …（密度 ×3~×8、无逐格堆指针）。
//!
//! ## 类型化去装箱列
//! 列类型由字段 schema 默认值在注册期定型：`Bool→Vec<bool>`、`Int→Vec<i64>`、
//! `Float→Vec<f64>`、`Vec3→Vec<[f64;3]>`、`Quat→Vec<[f64;4]>`，其余（Str/Ref/Map/Null）
//! 落 [`Column::Boxed`]。收益无条件正（白送）：列扫描密度高、无逐格堆指针、快照即连续
//! memcpy。诚实条款：若某格写入与列类型不符（如向 Int 列写 Float/Null），该列一次性
//! **去优化**为 `Boxed`（保精确往返、不染邻行），正确性永不受损。

use crate::value::Value;

/// 类型化无装箱列。列类型由字段 schema 默认值在注册期定型；类型不符的写入
/// 触发一次性[去优化][`Column::boxify`]到 `Boxed`（保精确往返）。
///
/// 本内核按行号寻址、与生命周期无关——上层的行/存活内核（sim 的代际 + [`RowPolicy`]
/// 或 render 的 id 直址位图）自行决定哪个 row 属于哪个实例。
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
    /// 按字段默认值定型并填充 `n` 行。Bool/Int/Float/Vec3/Quat 走无装箱列，其余落 Boxed。
    pub(crate) fn with_default(default: &Value, n: usize) -> Column {
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
    pub(crate) fn get(&self, row: usize) -> Value {
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
    pub(crate) fn set(&mut self, row: usize, v: Value) -> bool {
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
                return true;
            }
        }
        false
    }

    /// 追加一行。
    pub(crate) fn push(&mut self, v: Value) {
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

    /// 增长 / 截断到 `n` 行，新增格以 `default` 填充（render sidecar 被动跟随 sim
    /// 分配新 id 时扩列用）。`default` 与列类型不符则一次性去优化为 Boxed 再填。
    pub(crate) fn resize(&mut self, n: usize, default: &Value) {
        match (&mut *self, default) {
            (Column::Bool(c), Value::Bool(b)) => c.resize(n, *b),
            (Column::Int(c), Value::Int(i)) => c.resize(n, *i),
            (Column::Float(c), Value::Float(f)) => c.resize(n, *f),
            (Column::Vec3(c), Value::Vec3(a)) => c.resize(n, *a),
            (Column::Quat(c), Value::Quat(a)) => c.resize(n, *a),
            (Column::Boxed(c), _) => c.resize(n, default.clone()),
            _ => {
                self.boxify();
                if let Column::Boxed(c) = self {
                    c.resize(n, default.clone());
                }
            }
        }
    }

    pub(crate) fn swap_remove(&mut self, row: usize) {
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
    pub(crate) fn clear_row(&mut self, row: usize) {
        if let Column::Boxed(c) = self {
            c[row] = Value::Null;
        }
    }

    /// `_alive` / 其他 Bool 列的稠密位扫描入口（白送优化「ECS 快路 / type 扇出」
    /// 顺序列访问）。非 Bool（已去优化）返回 None，调用方回退逐格 get。
    pub(crate) fn as_bool_slice(&self) -> Option<&[bool]> {
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
