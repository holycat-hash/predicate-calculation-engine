# 05 冷却与状态机

## 问题

「技能冷却 60 帧；施法请求在冷却中要被拒绝。角色状态机 idle/casting/stunned，
非法转移（stunned 期间施法）不得发生。」

## 为什么刁钻

「冷却中」是一段**时间区间**，直觉做法是每帧检查计时器——退回轮询。
「非法转移」直觉做法是在 calculation 里写 if——能跑，但拦截晚了：
触发已经发生，O(|F|) 预算已经花掉，且每个 calc 都要重复防御性检查。

## 切分

两个手法：**事件带帧戳**（写者盖戳，读者比戳）+ **守卫上移到谓词层**
（condition 本来就是「值得醒吗」的过滤器，§3.1）。

- **entity** `Unit`：字段 `cast_req`（结构：技能+帧戳）、`cd_until`、`state`。
- **calculation** `request_calc`：产生请求时快照读 `Clock.frame` 盖戳，
  写 `own(cast_req) = {skill: s, frame: F}`。
- **calculation** `cast_calc`：唯一写 `cd_until` 的人；谓词层已挡掉冷却中与非法状态。

## 谓词代数

```
# 施法：冷却与状态守卫全部在 condition——被拒的请求连 calculation 都不触发
on    own(cast_req)
where cmp(new.frame, ≥, own.cd_until) and own.state in {"idle", "moving"}
each  deliver(new.skill)
→ cast_calc            # 写 own(casting_skill)；写 own(cd_until) = new.frame + 60

# 状态机转移：每个转移 = 一条谓词 + 同一个状态 calc（D1：state 单写者）
on    own(stun_hit)                       # 被击晕事件
where cmp(own.state, ≠, "dead")
each
→ state_calc           # 写 own(state) = "stunned"

on    own(stun_until_passed)              # 解除事件（由 01 的租约/时间戳模式产生）
where became(true)
each
→ state_calc           # 写 own(state) = "idle"
```

## 正确性论证

- 无轮询：冷却期间**零成本**——没有任何谓词被叫醒，直到下一次 `cast_req` 写入。
  对比计时器递减方案（每帧每单位一次写+一次路由）。
- 帧戳偏移：request_calc 读到的 `Clock.frame` 是上一帧值（快照读），
  `cd_until` 同源同偏移，比较封闭，不漂移。
- 守卫即文档：合法转移条件全部摊在谓词里，注册期可见可审计；
  calculation 内不需要防御性 if。
- D1 的状态机红利：`state` 唯一归 state_calc——所有转移必然汇于一处，
  「两个系统同时改状态」这类 bug 在注册期就报错。
- 单谓词制（§1.4）与多转移源不冲突：多事件源用 scope 并（`|`）合流进同一谓词,
  或如上为不同 condition 各设谓词、喂**不同** calculation，再由它们各写一个
  请求字段、state_calc 订阅请求字段合流（聚合先物化，§1.4 出路三）。

## 成本

own scope 哈希链 O(1)；own 字段守卫是点查 O(1)（活阈值仅自己一个订阅者，不退化）。
