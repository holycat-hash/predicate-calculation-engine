//! entity 层：实例化的最小单位，写作 `entityname.id`（§1.2）。
//!
//! - 所有数据属于某个 entity 实例；全局性状态以 singleton entity 表达（如 Grid.0、Clock.0）。
//! - entity 本身没有行为；行为全部在挂于其类型之下的 calculation。
//! - id 无顺序语义、可复用；复用安全性由内部代际号保证（§6.3），对用户不可见。

use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityTypeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FieldId(pub u32);

/// 实例标识 = 类型 + id + 代际号。代际号防 ABA：id 归还复用后，
/// 旧帧残留的 ref 值因代际不同而不会误指新住户。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstanceId {
    pub ty: EntityTypeId,
    pub id: u32,
    pub(crate) generation: u64,
}

/// cell 地址：(type, id, field)。写入、订阅、路由的最小粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellAddr {
    pub inst: InstanceId,
    pub field: FieldId,
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    /// ref 类型字段：runtime 为其维护反向表，结算时写 null（§6.3）。
    pub is_ref: bool,
    pub default: Value,
}

impl FieldDef {
    pub fn new(name: &str, default: Value) -> Self {
        FieldDef {
            name: name.to_string(),
            is_ref: false,
            default,
        }
    }

    pub fn reference(name: &str) -> Self {
        FieldDef {
            name: name.to_string(),
            is_ref: true,
            default: Value::Null,
        }
    }
}

/// entity 类型的 schema。注册期定型。
#[derive(Debug, Clone)]
pub struct EntityType {
    pub name: String,
    /// `fields[0]` 恒为内建 `_alive`（runtime 注册时自动注入）。
    pub fields: Vec<FieldDef>,
    /// singleton 类型恒有且只有实例 0。
    pub singleton: bool,
}

/// 内建字段 `_alive` 的 FieldId（每个类型的 0 号字段）。
/// 创建：runtime 写 `_alive = true`；销毁唯一入口：自己写 `_alive = false`（§6.3）。
pub const FIELD_ALIVE: FieldId = FieldId(0);
