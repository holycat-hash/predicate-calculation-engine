//! cell 的值类型。cell 是 entity 实例的一个字段，是数据、写入与订阅的最小粒度。
//!
//! ref 是 runtime 认识的 cell 类型（§6.3）：携带内部代际号防 ABA，
//! 代际号对用户不可见（比较时参与判等，杜绝旧 ref 误指新住户）。

use std::collections::BTreeMap;

use crate::entity::InstanceId;

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Value {
    /// ref 被结算清空、或字段尚未写入时的值；`became(null)` 用它收尸。
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// 实例引用。含代际号（InstanceId 内部），判等时参与比较。
    Ref(InstanceId),
    /// 结构化 cell，条件允许字段路径（如 `new.target`，§3.3）。
    Map(BTreeMap<String, Value>),
    /// 三维向量（平移 / 缩放 / 轴）。类型化去装箱：内联 `[f64;3]`，不经 Map 堆
    /// 分配；存储侧落专属 SoA 列（[`crate::runtime`] 的 `Column::Vec3`），render 的
    /// `Vec3Lerp` 在其上分量线性插值。分量经路径读取（`pos.x`/`.y`/`.z`）。
    Vec3([f64; 3]),
    /// 单位四元数 `(x, y, z, w)`，表方向 / 旋转。分量线性插值会变速且离开单位球
    /// （视觉错误），故单列一型由 render 的 `Slerp` 球面插值。分量路径 `.x/.y/.z/.w`。
    Quat([f64; 4]),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(s.to_string())
    }

    pub fn map<const N: usize>(pairs: [(&str, Value); N]) -> Value {
        Value::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// 三维向量字面量（平移 / 缩放 / 轴）。
    pub fn vec3(x: f64, y: f64, z: f64) -> Value {
        Value::Vec3([x, y, z])
    }

    /// 四元数字面量 `(x, y, z, w)`。一般应为单位四元数（render 的 Slerp 假定单位）。
    pub fn quat(x: f64, y: f64, z: f64, w: f64) -> Value {
        Value::Quat([x, y, z, w])
    }

    /// 单位四元数（无旋转）`(0,0,0,1)`——旋转字段的常用默认值。
    pub fn quat_identity() -> Value {
        Value::Quat([0.0, 0.0, 0.0, 1.0])
    }

    pub fn as_vec3(&self) -> Option<[f64; 3]> {
        match self {
            Value::Vec3(a) => Some(*a),
            _ => None,
        }
    }

    pub fn as_quat(&self) -> Option<[f64; 4]> {
        match self {
            Value::Quat(a) => Some(*a),
            _ => None,
        }
    }

    /// 沿字段路径取值；路径为空返回自身。路径落空返回 Null。
    /// Vec3/Quat 的分量（`x`/`y`/`z`/`w`）是终端标量，可被路径直读——谓词条件
    /// 与 render 反应投影因此能引用 `new.pos.x` 这类单分量。
    pub fn get_path(&self, path: &[String]) -> Value {
        let mut cur = self;
        let mut rest = path;
        while let Some((seg, tail)) = rest.split_first() {
            match cur {
                Value::Map(m) => match m.get(seg) {
                    Some(v) => cur = v,
                    None => return Value::Null,
                },
                Value::Vec3(a) => return component(a, seg, tail),
                Value::Quat(a) => return component(a, seg, tail),
                _ => return Value::Null,
            }
            rest = tail;
        }
        cur.clone()
    }

    /// 数值视图：Int/Float/Bool 可比较与四则运算的统一通道。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_ref_id(&self) -> Option<InstanceId> {
        match self {
            Value::Ref(r) => Some(*r),
            _ => None,
        }
    }

    /// 谓词条件里的比较语义：数值跨类型按 f64 比，其余类型仅支持判等。
    pub fn cmp_num(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self.as_f64(), other.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b),
            _ => None,
        }
    }
}

/// 向量/四元数分量取值：`x`/`y`/`z`(`/w`) → Float；非终端段（标量后再深入）或
/// 越界 → Null（与「在标量上继续取路径」同语义）。
fn component(a: &[f64], seg: &str, tail: &[String]) -> Value {
    if !tail.is_empty() {
        return Value::Null;
    }
    let i = match seg {
        "x" => 0,
        "y" => 1,
        "z" => 2,
        "w" => 3,
        _ => return Value::Null,
    };
    a.get(i).map_or(Value::Null, |&f| Value::Float(f))
}

// ---- 字面量直写便利：便于示例、测试和外部调用直接传标量 ----

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v as i64)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}

impl From<InstanceId> for Value {
    fn from(v: InstanceId) -> Self {
        Value::Ref(v)
    }
}

impl From<[f64; 3]> for Value {
    fn from(v: [f64; 3]) -> Self {
        Value::Vec3(v)
    }
}

impl From<[f64; 4]> for Value {
    fn from(v: [f64; 4]) -> Self {
        Value::Quat(v)
    }
}

impl From<()> for Value {
    fn from(_: ()) -> Self {
        Value::Null
    }
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}
