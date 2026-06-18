//! sim → render 的发布握手（并发双线程，你的选择）。
//!
//! sim 在自己的线程上 `step()`；每帧边界把一份**不可变** [`SimFrame`] 发布出去。
//! render 在自己的（动态帧率）线程上持有当前 `Arc<SimFrame>`，跨多个 render 帧
//! 反复插值，直到它愿意取下一帧。sim 抢跑构建下一帧时不触碰已发布的那份——
//! 不可变 + Arc 即并发安全，无细粒度同步（A7）。
//!
//! 关键：[`SimFrame`] 只携**变化的** tracked 增量（写日志天然的稀疏集，§2）+
//! 事件写日志 + 生灭增量，是 O(|Δ|) 而非克隆整库。render 自己的 sidecar 维持
//! tracked 镜像，逐帧增量套用（A8 的稀疏性在并发下照样成立）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::entity::{EntityTypeId, FIELD_ALIVE, FieldId, InstanceId};
use crate::runtime::{Runtime, WriteRec};
use crate::value::Value;

/// 一个被 track 的 sim 字段在某实例上的本帧增量。
#[derive(Debug, Clone)]
pub struct TrackedDelta {
    pub inst: InstanceId,
    pub sim_field: FieldId,
    pub old: Value,
    pub new: Value,
}

/// sim 帧 N 提交后发布的不可变快照。render 消费它，永不回写。
#[derive(Debug, Clone, Default)]
pub struct SimFrame {
    /// 本快照对应的 sim 帧号（render 据此判定 tracked cell「本区间是否在动」）。
    pub sim_frame: u64,
    /// 本 sim 帧变化的 tracked cell（稀疏，O(|Δ|)）。
    pub tracked: Vec<TrackedDelta>,
    /// 本 sim 帧的全部提交写（render 事件反应的触发源，复用谓词代数路由）。
    pub events: Vec<WriteRec>,
    /// 本 sim 帧出生的实例（`_alive` 写真）。
    pub births: Vec<InstanceId>,
    /// 本 sim 帧死亡的实例（`_alive` 写假）。
    pub deaths: Vec<InstanceId>,
}

/// 发布器：sim 线程 `publish`，render 线程 `drain`。
///
/// **唯一消费路径是 `drain`**（顺序取走全部未消费帧），因为出生/死亡/事件**不可丢**
/// ——render 跳帧（启动慢 / 比 sim 慢）若只取「最新一帧」就会漏掉中间帧的生灭与反应，
/// 留下永不回收的 render 幽灵、或永不出现的实体。故不提供「只 peek 最新」的入口（那是
/// 脚枪：既漏生灭，又因不出队列而无界增长 OOM）。render 每帧 `drain` 全部并逐帧
/// `ingest`：生灭事件无丢失，插值自然落在最后一帧的区间。sim 远快于 render 时：所有
/// 事件照常处理，只画最新插值态——正是「render 跟不上」时该有的行为（DF3）。
///
/// **队列界（B5）。** 队列长度 = 未 drain 的 sim 帧数：render 每帧 drain ⇒ 常态有界；
/// 但 render **真停摆**（线程阻塞 / 长时间不 drain）时队列按 sim 帧数线性增长。缓解留作
/// seam——把跳过的帧**合并**（生灭 / 事件按序追加、tracked 增量 latest-wins 折叠）成更少
/// 的 `SimFrame`，令停摆期内存与「跳过多少帧」无关、只与「多少实体在动」有关；v1 未做
/// （常态每帧 drain 不触发）。
///
/// 用 `Mutex<Vec>` 守一次队列操作，临界区只是 Vec 的 push / swap——render 取走后整个
/// 插值区间不再加锁。
pub struct Publisher {
    /// render 关心的 tracked cell 集（注册期定型）：(类型, sim 字段)。
    tracked_fields: Vec<(EntityTypeId, FieldId)>,
    /// 未被 render 消费的已发布帧（顺序）。长度 = 未 drain 的 sim 帧数（见 [`Publisher`] 队列界 B5）。
    queue: Mutex<Vec<Arc<SimFrame>>>,
}

impl Publisher {
    pub fn new(tracked_fields: Vec<(EntityTypeId, FieldId)>) -> Self {
        Publisher {
            tracked_fields,
            queue: Mutex::new(vec![]),
        }
    }

    /// 某 (类型, 字段) 是否被 track（决定是否进 tracked 增量）。
    fn is_tracked(&self, ty: EntityTypeId, f: FieldId) -> bool {
        self.tracked_fields
            .iter()
            .any(|&(t, ff)| t == ty && ff == f)
    }

    /// 从 sim runtime 当前帧的提交写集构建一份 [`SimFrame`]，发布给 render。
    /// 在 sim 线程、`rt.step()` 之后调用（此刻 `committed_writes` = 本帧写日志）。
    /// 帧号直接取自 [`Runtime::frame`]，避免调用方手填造成重复摄入或跳帧。
    pub fn publish(&self, rt: &Runtime) {
        let mut frame = SimFrame {
            sim_frame: rt.frame(),
            ..Default::default()
        };
        let mut tracked_of: HashMap<(InstanceId, FieldId), usize> = HashMap::new();
        let mut births = HashSet::new();
        let mut deaths = HashSet::new();
        for rec in rt.committed_writes() {
            if rec.field == FIELD_ALIVE {
                match &rec.new {
                    Value::Bool(true) if births.insert(rec.inst) => frame.births.push(rec.inst),
                    Value::Bool(false) if deaths.insert(rec.inst) => frame.deaths.push(rec.inst),
                    _ => {}
                }
            }
            if self.is_tracked(rec.inst.ty, rec.field) {
                let key = (rec.inst, rec.field);
                if let Some(&i) = tracked_of.get(&key) {
                    frame.tracked[i].new = rec.new.clone();
                } else {
                    tracked_of.insert(key, frame.tracked.len());
                    frame.tracked.push(TrackedDelta {
                        inst: rec.inst,
                        sim_field: rec.field,
                        old: rec.old.clone(),
                        new: rec.new.clone(),
                    });
                }
            }
            frame.events.push(rec.clone());
        }
        self.queue.lock().unwrap().push(Arc::new(frame));
    }

    /// render 线程取走全部未消费帧（顺序）。逐帧 `ingest` 以不丢生灭/事件；
    /// 插值落在最后一帧。无新帧则返回空。
    pub fn drain(&self) -> Vec<Arc<SimFrame>> {
        std::mem::take(&mut *self.queue.lock().unwrap())
    }
}
