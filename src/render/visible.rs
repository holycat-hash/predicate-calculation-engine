//! 可见集 / 剔除 / LOD：render 侧的 **§6.1「物化为索引实体」对偶**。
//!
//! sim 侧把空间查询物化为索引实体（calc 持网格、batch 订阅 position、把结果写进自己
//! 字段，见 `tests/spatial_index.rs`）。render 侧同理但消费者是 render 自己：
//! [`super::RenderRuntime`] 自维护一份 [`SpatialGrid`]（喂自它本就摄入的 tracked
//! position 增量），每 render 帧用相机查询网格得**可见集**，据此**收窄** `continuous`
//! 与 `submit` 两处 O(存活) 全扫——离屏实体不重算派生 render 态、不进提交。
//!
//! 这不是第四注册概念：行为仍是 track/reaction/continuous；可见集是一个**索引 + 查询**
//! （把扫描的 N 压到「相机视域内」），外加一个派生**距离字段**供开发者自行分档 LOD。
//!
//! ## 为什么放 render 侧（render-rate）
//! 相机动态、按 render 帧率平滑移动——这正是 render runtime 存在的理由。网格内容按
//! sim 帧率更新（position 增量稀疏喂入），相机 + 查询按 render 帧率运行：相机平滑移动时
//! 查询窗口每帧滑过网格，剔除决策每 render 帧刷新。cur 位与插值位的差用**剔除余量**
//! （cull 形状略大于实际视域）吸收，无 popping。
//!
//! ## LOD：暴露距离、开发者分档
//! 引擎只算「每可见实体到相机的距离」写进一个 render 字段（架构哲学：有代价优化给
//! 接口 + 上游，不替开发者定分档语义）。开发者读该字段、用 [`lod_band`] 或自定映射选
//! mesh / 简化度。`RenderBinding` 不为 LOD 加槽。

use crate::entity::InstanceId;
use crate::spatial::SpatialGrid;
use crate::value::Value;

/// 剔除区域形状（以相机投影点为中心，在网格 2D 平面内）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CullShape {
    /// 半径剔除（视域近似为圆，AoI 式）。
    Radius(f64),
    /// 轴对齐矩形剔除（半宽 `hx` / 半高 `hy`，正交相机 / 俯视框选式）。
    Aabb { hx: f64, hy: f64 },
}

impl CullShape {
    /// 以 (cx, cy) 为中心对网格做广相位查询，返回区域内全部住户（确定序，含各剔除类型，
    /// 由调用方按类型分桶）。
    pub(super) fn query(&self, grid: &SpatialGrid, cx: f64, cy: f64) -> Vec<InstanceId> {
        match *self {
            CullShape::Radius(r) => grid.query_radius(cx, cy, r),
            CullShape::Aabb { hx, hy } => grid.query_aabb(cx - hx, cy - hy, cx + hx, cy + hy),
        }
    }

    /// 形状参数是否合法（有限正数）。`enable_culling` 注册期校验。
    pub(super) fn is_valid(&self) -> bool {
        match *self {
            CullShape::Radius(r) => r.is_finite() && r > 0.0,
            CullShape::Aabb { hx, hy } => hx.is_finite() && hx > 0.0 && hy.is_finite() && hy > 0.0,
        }
    }
}

/// 把 3D 平移（[`Value::Vec3`]，或带 `x/y/z` 路径的 [`Value::Map`]）投影到网格 2D 平面：
/// 选哪两个分量。3D 俯视 / 地面游戏常用 [`Axes::XZ`]，纯 2D 用 [`Axes::XY`]。全局（一张
/// 网格一个平面），相机与各剔除类型共用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axes {
    XY,
    XZ,
    YZ,
}

impl Axes {
    #[inline]
    fn idx(self) -> (usize, usize) {
        match self {
            Axes::XY => (0, 1),
            Axes::XZ => (0, 2),
            Axes::YZ => (1, 2),
        }
    }

    #[inline]
    fn names(self) -> (&'static str, &'static str) {
        match self {
            Axes::XY => ("x", "y"),
            Axes::XZ => ("x", "z"),
            Axes::YZ => ("y", "z"),
        }
    }

    /// 投影一个平移值到 2D。`Vec3` 走内联数组快路径；`Map` 退化经 `get_path` 取分量。
    /// 任一分量取不到数值则 `None`（该实体本帧不喂网格 / 相机缺位不剔除）。
    #[inline]
    pub(super) fn project(self, v: &Value) -> Option<(f64, f64)> {
        if let Some(a) = v.as_vec3() {
            let (i, j) = self.idx();
            return finite_pair(a[i], a[j]);
        }
        let (a, b) = self.names();
        finite_pair(
            v.get_path(&[a.to_string()]).as_f64()?,
            v.get_path(&[b.to_string()]).as_f64()?,
        )
    }
}

#[inline]
fn finite_pair(x: f64, y: f64) -> Option<(f64, f64)> {
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// 把到相机的距离映射到 LOD 档号：`thresholds` 须升序，返回第一个 `dist < t` 的档下标，
/// 都不小于则末档（`thresholds.len()`）。纯函数——LOD 分档策略由开发者持有，引擎只暴露
/// 距离与这个可选便利映射。
///
/// 例：`thresholds = [10, 50, 200]` ⇒ dist 5→0（最精）、30→1、100→2、500→3（最简）。
pub fn lod_band(dist: f64, thresholds: &[f64]) -> usize {
    thresholds
        .iter()
        .position(|&t| dist < t)
        .unwrap_or(thresholds.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lod_band_maps_distance_to_level() {
        let th = [10.0, 50.0, 200.0];
        assert_eq!(lod_band(5.0, &th), 0);
        assert_eq!(lod_band(10.0, &th), 1); // 边界：dist == t 落入下一档（t 是该档上界开区间）
        assert_eq!(lod_band(30.0, &th), 1);
        assert_eq!(lod_band(100.0, &th), 2);
        assert_eq!(lod_band(500.0, &th), 3);
        assert_eq!(lod_band(0.0, &[]), 0); // 空阈值：恒末档 0
    }

    #[test]
    fn axes_project_vec3_and_map() {
        let v = Value::vec3(1.0, 2.0, 3.0);
        assert_eq!(Axes::XY.project(&v), Some((1.0, 2.0)));
        assert_eq!(Axes::XZ.project(&v), Some((1.0, 3.0)));
        assert_eq!(Axes::YZ.project(&v), Some((2.0, 3.0)));
        let m = Value::map([
            ("x", Value::Float(4.0)),
            ("y", Value::Float(5.0)),
            ("z", Value::Float(6.0)),
        ]);
        assert_eq!(Axes::XZ.project(&m), Some((4.0, 6.0)));
        assert_eq!(Axes::XY.project(&Value::Null), None);
    }

    #[test]
    fn cull_shape_validity() {
        assert!(CullShape::Radius(5.0).is_valid());
        assert!(!CullShape::Radius(0.0).is_valid());
        assert!(!CullShape::Radius(f64::NAN).is_valid());
        assert!(CullShape::Aabb { hx: 1.0, hy: 2.0 }.is_valid());
        assert!(!CullShape::Aabb { hx: 1.0, hy: -1.0 }.is_valid());
    }
}
