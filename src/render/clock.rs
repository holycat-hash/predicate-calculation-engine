//! render 时钟：动态帧率的入口（与 sim 的整数 `Clock` 对偶）。
//!
//! sim 的 `Clock.frame` 是单调整数 +1，时长语义建立在帧差上。render 帧率可变，
//! 故 render 时钟多两格实数：
//! - `dt`：本 render 帧的真实经过秒数。一切按时间积分的视觉量（粒子 age、
//!   缓动进度、相机阻尼）读它，**不读帧数**。
//! - `alpha`：插值主参数 = `accumulator / sim_dt ∈ [0,1)`，指明「当前 render 帧
//!   落在上一 sim 帧与当前 sim 帧之间的哪个位置」。sim 步进时归零，render 帧推进。
//!   alpha ≥ 1 即进入外推区（Cr2，本版不主动外推，钳到 1）。
//!
//! 时钟不是订阅源的「魔法」，它就是 render runtime 每帧持有的三个可读量；连续
//! calc 经 [`crate::render::RenderCtx`] 读取（ECS 快路是 render 的主热路径，
//! 与 sim 把轮询打入冷宫恰好相反）。

/// render 帧的时间状态。每个 render 帧由宿主循环填入。
#[derive(Debug, Clone, Copy)]
pub struct RenderClock {
    /// 已推进的 render 帧数（单调 +1，仅供遥测 / 调试，时长语义勿用它）。
    pub frame: u64,
    /// 本 render 帧经过秒数（动态帧率：每帧不同）。
    pub dt: f64,
    /// 插值因子 ∈ [0,1]：当前 render 帧在 (上一 sim 帧, 当前 sim 帧) 之间的位置。
    pub alpha: f64,
}

impl RenderClock {
    pub fn new() -> Self {
        RenderClock {
            frame: 0,
            dt: 0.0,
            alpha: 0.0,
        }
    }

    /// 宿主循环每 render 帧调用：推进帧号、记录 dt、钳定 alpha 到 [0,1]
    /// （越界即外推区，本版钳到端点——平顺优先，不过冲，Cr2）。
    pub fn begin_frame(&mut self, dt: f64, alpha: f64) {
        self.frame += 1;
        self.dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
    }
}

impl Default for RenderClock {
    fn default() -> Self {
        Self::new()
    }
}
