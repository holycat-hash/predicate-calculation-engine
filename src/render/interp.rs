//! 插值原语：`track` 是 render 侧的 `fold`。
//!
//! `fold`（sim）让 runtime 增量维护一个聚合视图，calc 不必每帧 O(N) 扫描。
//! `interp`（render）让 runtime 增量维护一个 (prev, cur) 双缓冲 + 按 alpha 求值，
//! calc 不必手算插值。同一个「视图即数据、增量维护」定理，换一根轴：
//! - `fold` 的输入是写流，输出是聚合；
//! - `interp` 的输入是上一 sim 帧→当前 sim 帧的逐 cell 增量（写日志天然携带的
//!   `(old, new)`，§2 双缓冲免费），输出是 `sample(prev, cur, alpha)`。
//!
//! 插值种类必须由开发者每字段声明（Cr1）：选错出视觉瑕疵（lerp 一次传送 = 实体
//! 滑过全图）。默认 `Snap`（取 cur，无插值，免费）；平顺处显式开 `Lerp`。

use crate::value::Value;

/// 插值种类（Cr1，每 tracked 字段二选一/三选一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interp {
    /// 直接取当前 sim 帧值，不插值。离散量（精灵帧、朝向枚举）与默认选择。
    Snap,
    /// 线性插值 `prev + (cur - prev) * alpha`。位置、缩放等连续数值量。
    Lerp,
    /// 阶跃：alpha < 1 取 prev，alpha = 1 取 cur。布尔 / 不可中间态的量。
    Step,
}

impl Interp {
    /// 按 alpha 在 (prev, cur) 上求值。非数值量一律退化为 `Snap`（取 cur）。
    pub fn sample(self, prev: &Value, cur: &Value, alpha: f64) -> Value {
        match self {
            Interp::Snap => cur.clone(),
            Interp::Step => {
                if alpha >= 1.0 {
                    cur.clone()
                } else {
                    prev.clone()
                }
            }
            Interp::Lerp => match (prev.as_f64(), cur.as_f64()) {
                (Some(p), Some(c)) => Value::Float(p + (c - p) * alpha),
                // 非数值无中间态：退化为 Snap，不报错（视觉量容错）。
                _ => cur.clone(),
            },
        }
    }
}

/// 一条 tracked 声明：把某 sim 字段镜像进 render，按 kind 维护插值输出字段。
///
/// `sim_field` 属 sim 命名空间（render 只读，经写日志增量得到 prev/cur）；
/// `out` 属 render 命名空间（render 独占写，D1 render 侧）。
#[derive(Debug, Clone, Copy)]
pub struct Track {
    pub ty: crate::entity::EntityTypeId,
    /// 被镜像的 sim 字段（sim 命名空间）。
    pub sim_field: crate::entity::FieldId,
    /// 插值结果写入的 render 字段（render 命名空间）。
    pub out: super::store::RFieldId,
    /// 该字段在 [`super::store::RenderStore`] 中的局部 track 槽位（prev/cur 列下标）。
    pub slot: usize,
    pub kind: Interp,
}
