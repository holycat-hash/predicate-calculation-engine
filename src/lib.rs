//! PredicateCalculationEngine (PCE)
//!
//! 四层抽象：runtime / entity / calculation / predicate（见 docs-zh/PCE文档.md）。
//! 任何新需求必须折叠进这四层，禁止引入第五种概念。
//!
//! - 唯一触发源：上一帧的 write（D2 写即事件）
//! - 单写者制（D1）：字段静态归属唯一 calculation，注册期检查
//! - 快照读：本帧写入本帧不可见（双缓冲 = 单存储 + 写日志）
//! - 交付无序（D3）：batch 是多重集，消费逻辑必须顺序无关
//! - 成本不变量：整帧调度 O(|W|·log + |F|)，与谓词总数、实例总数无关
//!
//! ## 快速上手（核心 API）
//!
//! ```
//! use pce::predicate::type_scope;
//! use pce::{
//!     Cond, Delivery, FieldDef, FoldOp, Predicate, Runtime, Value,
//! };
//!
//! let mut rt = Runtime::new();
//! let enemy = rt.register_entity_type(
//!     "Enemy",
//!     vec![FieldDef::new("hp", Value::Int(0))],
//!     false,
//! );
//! let ui = rt.register_entity_type(
//!     "BossBar",
//!     vec![FieldDef::new("total_hp", Value::Float(0.0))],
//!     true,
//! );
//!
//! // §7 示例 4：Boss 血条——增量聚合，每写 ±delta 而非每帧 O(N) 扫描
//! let f_hp = rt.field(enemy, "hp");
//! let f_total = rt.field(ui, "total_hp");
//! rt.register_calculation(
//!     "boss_bar",
//!     ui,
//!     Predicate::new(type_scope(enemy, f_hp), Cond::True, Delivery::Fold(FoldOp::Sum)),
//!     &[f_total],
//!     Box::new(move |ctx, input| ctx.write(f_total, input.agg().clone())),
//! )
//! .unwrap();
//!
//! let e = rt.spawn(enemy, vec![(f_hp, Value::Int(100))]);
//! rt.step(); // 路由 spawn 写集 → boss_bar 触发并提交
//! assert_eq!(rt.read(rt.alive(ui)[0], f_total), Value::Float(100.0));
//! # let _ = e;
//! ```
//!
//! ## 优化档位
//!
//! 白送优化（A 层）无条件生效：SoA 列存、写日志双缓冲、值桶 / 共享排序
//! 阈值表、等价条件合并、fold 增量维护、ECS 快路、scratch 复用、免费
//! profiler（[`runtime::Profile`]）。开发者档位（C 层）：
//!
//! | 档位 | 入口 | 代价 |
//! |---|---|---|
//! | C1 执行档位 | [`runtime::Tier`] / `.tier(Kernel)` | 表达力受限 + 发散风险自负 |
//! | C2 读集声明 | `.reads([...])` | 声明负担；不声明退化为 profile 猜测 |
//! | C3 驻留划分 | [`runtime::Residency`] / `.residency(...)` | 没有静态正解，pin 自负 |
//! | C4 确定性 | [`runtime::Determinism`] / `set_determinism` | 规范序的排序成本 |
//! | C5 检测档位 | [`runtime::Detect`] / `set_detect` | Strict/Warn 污染热路径 |
//! | C6 行身份 | [`runtime::RowPolicy`] / `.compact()` | 稳定行留洞 vs 压缩行重映射 |
//!
//! 灰区（B 层）自适应所需的遥测由 [`runtime::Profile`] 零边际成本提供（D2 买单）。

pub mod calculation;
pub mod entity;
pub mod predicate;
pub mod runtime;
pub mod value;

pub use calculation::{CalcId, Ctx, Input};
pub use entity::{CellAddr, EntityTypeId, FieldDef, FieldId, InstanceId};
pub use predicate::{CmpOp, Cond, Delivery, Dir, Expr, FoldOp, Predicate, Proj, Scope, ValRef};
pub use runtime::{CalcOptions, Detect, Determinism, Profile, Residency, RowPolicy, Runtime, Tier};
pub use value::Value;
