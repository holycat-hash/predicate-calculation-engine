//! 时钟与每帧逻辑（§6.2）：runtime 作为 writer。
//!
//! runtime 每帧向内建 cell `Clock.frame` 写帧号。订阅它等价于轮询——
//! 代价存在，但显式、可见、自付。这是「每帧都要跑」逻辑的唯一合法出口。
//!
//! 定时语义经 alarm：到点 runtime 写 `Clock.alarm = payload`，订阅者 each 触发。
//! 接口：`set_alarm`（绝对帧）/ `set_alarm_in`（相对帧）；此处用按帧号桶的简化 timer wheel，O(1)/帧摊销。

use std::collections::HashMap;

use crate::entity::{EntityTypeId, FieldId, InstanceId};
use crate::value::Value;

use super::{Store, WriteRec};

#[derive(Clone)]
pub struct Clock {
    pub ty: EntityTypeId,
    pub inst: InstanceId,
    pub f_frame: FieldId,
    pub f_alarm: FieldId,
    alarms: HashMap<u64, Vec<Value>>,
}

impl Clock {
    pub(crate) fn placeholder() -> Self {
        Clock {
            ty: EntityTypeId(0),
            inst: InstanceId {
                ty: EntityTypeId(0),
                id: 0,
                generation: 0,
            },
            f_frame: FieldId(0),
            f_alarm: FieldId(0),
            alarms: HashMap::new(),
        }
    }

    pub(crate) fn new(
        ty: EntityTypeId,
        inst: InstanceId,
        f_frame: FieldId,
        f_alarm: FieldId,
    ) -> Self {
        Clock {
            ty,
            inst,
            f_frame,
            f_alarm,
            alarms: HashMap::new(),
        }
    }

    pub(crate) fn set_alarm(&mut self, at_frame: u64, payload: Value) {
        self.alarms.entry(at_frame).or_default().push(payload);
    }

    /// 每帧由 runtime 调用：产出 Clock.frame 写；到点的 alarm 逐条产出写
    /// （D2 写即事件：多条 alarm 是多条 write，各自触发订阅者）。
    /// 注意这里只生成 write log，不提前改 store，保证本帧 calculation 仍读到上一帧快照。
    pub(crate) fn tick(&mut self, frame: u64, store: &Store, w: &mut Vec<WriteRec>) {
        let old = store.read(self.inst, self.f_frame);
        let new = Value::Int(frame as i64);
        w.push(WriteRec {
            inst: self.inst,
            field: self.f_frame,
            old,
            new,
        });
        if let Some(payloads) = self.alarms.remove(&frame) {
            let old = store.read(self.inst, self.f_alarm);
            for p in payloads {
                w.push(WriteRec {
                    inst: self.inst,
                    field: self.f_alarm,
                    old: old.clone(),
                    new: p,
                });
            }
        }
    }
}
