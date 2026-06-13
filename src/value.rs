//! cell 的值类型。cell 是 entity 实例的一个字段，是数据、写入与订阅的最小粒度。
//!
//! ref 是 runtime 认识的 cell 类型（§6.3）：携带内部代际号防 ABA，
//! 代际号对用户不可见（比较时参与判等，杜绝旧 ref 误指新住户）。

use std::collections::BTreeMap;

use crate::entity::InstanceId;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// ref 被结算清空、或字段尚未写入时的值；`became(null)` 用它收尸。
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// 实例引用。含代际号（InstanceId 内部），判等时参与比较。
    Ref(InstanceId),
    /// 结构化 cell，条件允许字段路径（如 `new.target`，§3.3）。
    Map(BTreeMap<String, Value>),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(s.to_string())
    }

    pub fn map<const N: usize>(pairs: [(&str, Value); N]) -> Value {
        Value::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// 沿字段路径取值；路径为空返回自身。路径落空返回 Null。
    pub fn get_path(&self, path: &[String]) -> Value {
        let mut cur = self;
        for seg in path {
            match cur {
                Value::Map(m) => match m.get(seg) {
                    Some(v) => cur = v,
                    None => return Value::Null,
                },
                _ => return Value::Null,
            }
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

impl Default for Value {
    fn default() -> Self {
        Value::Null
    }
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
