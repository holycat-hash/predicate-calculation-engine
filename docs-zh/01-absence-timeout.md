# 01 超时 / 静默 N 帧 / 心跳掉线

## 问题

「Session 若 30 帧内没有收到 Heartbeat，判定掉线。」

## 为什么刁钻

「没有写入」不是事件，是事件的缺席（§3.3）。谓词层禁止独立的 NOT 触发源——
否则系统退回每帧轮询。但「超时」业务上确实需要一个触发时刻。

## 切分

- **entity** `Session`：字段 `lease_until`（租约到期帧）、`state`。
- **entity** `Heartbeat 来源`（如 `Conn`）：写 `own(beat) = {session: ref, frame: F}`，
  写者盖帧戳（calculation 内快照读 `Clock.frame`，恒定一帧偏移，无碍）。
- **calculation** `renew_calc`（挂 Session）：收心跳，续租。
- **calculation** `expire_calc`（挂 Session）：判到期。时间触发源只有一个合法出口：
  订阅 `Clock.frame`（§6.2，显式轮询，代价自付）。

## 谓词代数

```
# 续租：正事件维护租约
on    type(Conn, beat)
where new.session = self
each  deliver(new.frame)
→ renew_calc                  # 写 own(lease_until) = new.frame + 30

# 到期：唯一合法的轮询者
on    type(Clock, frame)
where cmp(new, >, own.lease_until) and not cmp(own.state, =, "dead")
each
→ expire_calc                 # 写 own(state) = "dead"（或 destroy_self）
```

## 正确性论证

- 否定限制不破：`and not` 只是守卫，正触发源是 `Clock.frame`。
- 守卫 `own.state ≠ "dead"` 保证只触发一次（边沿化），否则到期后每帧重复触发。
- D1：`lease_until` 归 renew_calc，`state` 归 expire_calc，无冲突。
- 快照读：expire_calc 看到的 `own.lease_until` 是上一帧值；心跳与到期同帧竞速时
  偏保守一帧——租约语义下可接受；要求精确则把 30 改为 31。

## 成本

`Clock.frame` 上的条件引用 own 字段 → 活阈值，退化为该 cell 订阅者数（§4 诚实退化条款）：
每帧 O(Session 数) 次 O(1) 判定。Session 海量时按手法 6 翻转设计：
建 `Alarm.0` 索引实体（singleton），batch 订阅 `type(Session, lease_until)` 在自己字段里
维护按到期帧分桶的 timer wheel，自己独占订阅 `Clock.frame` 每帧弹一桶（O(1)/帧），
到期集合经[事件实体化](06-event-materialization.md)派发。
