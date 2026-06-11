# 11 无敌与免疫

**语言：** [English](../docs/11-immunity-invincibility.md) | 中文

## 问题

「受击后无敌 30 帧；霸体免疫击退但照常吃伤害；对中毒永久免疫；
BOSS 在水晶存活期间免疫一切；被免疫挡下的攻击要飘『免疫!』字。」

## 为什么刁钻

- 无敌是「一段时间内对一类事件说不」。区间不轮询由
  [05](05-cooldown-state-machine.md) 的时间戳守卫解决；新刺在「说不」本身：
  谓词层拒绝最便宜（condition 本来就是过滤器），但**谓词层的拒绝是静默的**
  ——calculation 压根不触发，没人知道挡了一下，而飘字要求「拒绝有回执」。
- 同帧穿透：无敌由受击触发，快照读下同帧 5 发全看到「还没无敌」，全部进来
  ——「受击**后**」的「后」在帧粒度下需要重新定义。
- 免疫集合：「new.kind ∈ own.immune_set」是动态集合成员判定，谓词代数没有
  （InSet 只收常量集，§3.3），也不该有——绑不上索引（§3.5）。
- 他授免疫：「水晶活着 → BOSS 免疫」要读别人的行，condition 禁 join。

## 切分

四根刺四个手法：

1. **时间窗 = 时间戳守卫**（05）：`immune_until` own 字段，来袭事件带帧戳，
   路由层比戳剪掉——免疫期内不触发、无每帧成本。
2. **回执 = 互补守卫双谓词**：同一来袭流拆两条谓词，守卫严格互补——
   未免疫走结算，免疫中走回执。拒绝照样不烦结算，飘字只在免疫期有成本。
3. **他授 = 镜像进自己的行**：经 ref 把别人的状态塌缩成 own 字段，
   守卫只看自己（手法 3 + §6.3 收尸）。
4. **动态集合 = 进 calculation**：少而静态的类型用析取守卫摊在谓词层；
   动态集合在 calc 里过滤——表达力不够时进 calc，不翻词汇表（§3.5 反向应用）。

- **entity** `Unit`：`immune_until`、`immune_all`（bool 镜像）、`crystal`（ref）。
  settle_calc 写 `hp`、`immune_until`（受击起窗，自产自守）；
  immune_fx_calc 写 `blocked_fx`；phase_calc 写 `immune_all`、`crystal`。

## 谓词代数

```
# A 结算：免疫窗内的来袭在路由层被剪（不触发、不交付）
on    type(Attacker, attack_out)
where new.target = self
      and cmp(new.frame, ≥, own.immune_until)
      and not cmp(own.immune_all, =, true)
batch deliver(new)
→ settle_calc          # 净算（10）；写 own(immune_until) = 本批帧戳 + 30

# B 飘字：守卫与 A 严格互补（≥ 对 <、not 对 =），每事件恰被一方接住
on    type(Attacker, attack_out)
where new.target = self
      and (cmp(new.frame, <, own.immune_until) or cmp(own.immune_all, =, true))
batch deliver(new)
→ immune_fx_calc       # 写 own(blocked_fx)；逐攻击者回执经 06 事件实体化

# C 他授：水晶死亡沿 ref 自动塌缩为自己的事件（§6.3：destroy 把 ref 写 null）
on    own(crystal)
where became(null)
each
→ phase_calc           # 写 own(immune_all) = false
```

## 正确性论证

- 免疫期成本剖面：无每帧成本（无轮询）；每条来袭付 O(1) 活守卫判定，
  不触发、不交付。对比「触发后在 calc 里 return」，省的是 |F| 预算与交付。
- 同帧穿透是规格点不是 bug：settle 是 batch 净算，本帧多发与触发无敌的那发
  **同戳同窗**——「受击后无敌」在帧粒度的精确语义是「同戳净击后起窗」。
  要「单发起窗、同帧其余作废」就在 calc 内按 (dmg, salt) 取一、其余弃置
  （[02](02-same-frame-contention.md) 决胜键，D3 合规重述）。
- 互补性可审计：A、B 的守卫是同一组原子比较的精确二分（≥/<、not/或），
  注册期即可验证「无事件双收、无事件漏收」。
- 帧戳封闭：`immune_until` 由来袭帧戳推出，与后续来袭比较同源恒偏移（05 同款）。
- 霸体 = 分类守卫：击退与伤害本就是不同的 out 字段/不同 kind——各流的谓词
  各守各的窗（`knockback_until`），「免疫击退但吃伤害」自然落地；
  kind 少而静态时也可单流析取守卫（`(new.kind = "stun" and 戳ok) or …`），
  仍在代数内。永久免疫是常量守卫：`and not cmp(new.kind, =, "poison")`。
- 镜像单写者：`immune_all` 与 `crystal` 同归 phase_calc——水晶重生重指 ref
  与镜像翻转必然同主，无竞写；水晶死亡经 §6.3 的 ref 置 null 触达，
  不依赖水晶临终自报。
- 与 [09](09-buff-debuff-ledger.md) 的关系：免疫也可以记成书里一项
  （kind = "immune_all"，until 即窗），由 book_calc 物化出 `immune_until`
  字段供本篇守卫引用——限时免疫 buff 因此零新机制。

## 成本

来袭路由 = 等值桶 O(1)+命中；命中后的守卫为活阈值点查 O(1) × 2 条谓词
（§4 诚实退化条款：仅限该 cell 的订阅者数，不随全局涨）。own/inst 链 O(1)。
免疫期无任何每帧开销；fx 谓词只在免疫期产生触发，成本随被挡次数走
（|F| 诚实计费）。
