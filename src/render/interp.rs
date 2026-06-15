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

/// 插值种类（Cr1，每 tracked 字段按量纲选）。错配（如对 [`Value::Quat`] 用
/// [`Interp::Lerp`]）一律退化为 `Snap`（取 cur），不报错——视觉量容错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interp {
    /// 直接取当前 sim 帧值，不插值。离散量（精灵帧、朝向枚举、handle）与默认选择。
    Snap,
    /// 标量线性插值 `prev + (cur - prev) * alpha`。单数值量（hp 条、单轴位移）。
    Lerp,
    /// 阶跃：alpha < 1 取 prev，alpha = 1 取 cur。布尔 / 不可中间态的量。
    Step,
    /// 三维向量分量线性插值（[`Value::Vec3`]）。平移、缩放。
    Vec3Lerp,
    /// 单位四元数球面线性插值（[`Value::Quat`]）。方向 / 旋转——分量 lerp 会变速
    /// 且离开单位球（角速度不均、缩放瑕疵），slerp 沿测地线匀速且保单位长。
    /// 取最短弧（必要时翻号，q 与 −q 同一旋转）；近平行退化为 nlerp（数值稳）。
    Slerp,
}

impl Interp {
    /// 按 alpha 在 (prev, cur) 上求值。量纲错配一律退化为 `Snap`（取 cur）。
    pub fn sample(self, prev: &Value, cur: &Value, alpha: f64) -> Value {
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
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
            Interp::Vec3Lerp => match (prev.as_vec3(), cur.as_vec3()) {
                (Some(p), Some(c)) => Value::Vec3([
                    p[0] + (c[0] - p[0]) * alpha,
                    p[1] + (c[1] - p[1]) * alpha,
                    p[2] + (c[2] - p[2]) * alpha,
                ]),
                _ => cur.clone(),
            },
            Interp::Slerp => match (prev.as_quat(), cur.as_quat()) {
                (Some(p), Some(c)) => Value::Quat(slerp(p, c, alpha)),
                _ => cur.clone(),
            },
        }
    }
}

/// 单位四元数球面线性插值，取最短弧。输入会先归一化，故非单位写入不会污染
/// 最短弧判定或输出长度；病态输入退化为可用端点 / identity。
/// near-parallel（`|dot|>0.9995`）退化为归一化线性插值——此处 `sinθ→0`，
/// slerp 公式数值不稳，nlerp 误差可忽略。
fn slerp(p: [f64; 4], c: [f64; 4], t: f64) -> [f64; 4] {
    let Some(p) = normalize(p) else {
        return normalize(c).unwrap_or([0.0, 0.0, 0.0, 1.0]);
    };
    let Some(mut c) = normalize(c) else {
        return p;
    };
    let mut dot = p[0] * c[0] + p[1] * c[1] + p[2] * c[2] + p[3] * c[3];
    // 最短弧：dot<0 说明走的是长弧，翻号 c（q 与 −q 表同一旋转）。
    if dot < 0.0 {
        c = [-c[0], -c[1], -c[2], -c[3]];
        dot = -dot;
    }
    if dot > 0.9995 {
        return normalize([
            p[0] + (c[0] - p[0]) * t,
            p[1] + (c[1] - p[1]) * t,
            p[2] + (c[2] - p[2]) * t,
            p[3] + (c[3] - p[3]) * t,
        ])
        .unwrap_or([0.0, 0.0, 0.0, 1.0]);
    }
    let theta_0 = dot.clamp(-1.0, 1.0).acos();
    let sin_0 = theta_0.sin();
    let s0 = ((1.0 - t) * theta_0).sin() / sin_0;
    let s1 = (t * theta_0).sin() / sin_0;
    [
        p[0] * s0 + c[0] * s1,
        p[1] * s0 + c[1] * s1,
        p[2] * s0 + c[2] * s1,
        p[3] * s0 + c[3] * s1,
    ]
}

/// 四元数归一化；零长度或非有限输入返回 None，由调用方选择容错端点。
fn normalize(q: [f64; 4]) -> Option<[f64; 4]> {
    if q.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let len = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if len < 1e-12 {
        None
    } else {
        Some([q[0] / len, q[1] / len, q[2] / len, q[3] / len])
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
