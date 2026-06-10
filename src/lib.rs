//! PredicateCalculationEngine (PCE)
//!
//! 四层抽象：runtime / entity / calculation / predicate（见 ../PCE文档.md）。
//! 任何新需求必须折叠进这四层，禁止引入第五种概念。
//!
//! - 唯一触发源：上一帧的 write（D2 写即事件）
//! - 单写者制（D1）：字段静态归属唯一 calculation，注册期检查
//! - 快照读：本帧写入本帧不可见（双缓冲）
//! - 交付无序（D3）：batch 是多重集，消费逻辑必须顺序无关
//! - 成本不变量：整帧调度 O(|W|·log + |F|)，与谓词总数、实例总数无关

pub mod calculation;
pub mod entity;
pub mod predicate;
pub mod runtime;
pub mod value;

pub use calculation::{CalcId, Ctx, Input};
pub use entity::{CellAddr, EntityTypeId, FieldDef, FieldId, InstanceId};
pub use predicate::{
    CmpOp, Cond, Delivery, Dir, Expr, FoldOp, Predicate, Proj, Scope, ValRef,
};
pub use runtime::Runtime;
pub use value::Value;
