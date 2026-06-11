# 18 吟唱、引导与打断

## 问题

「火球吟唱 90 帧、完成时结算；吟唱期间累计受伤 ≥50 或被晕立即打断，
移动主动取消；最后 10 帧不可打断；打断退还 70% 法力；
引导术每 30 帧一跳，打断保留已结算的跳。」

## 为什么刁钻

- 完成是**未来事件**：第 90 帧没有任何写入，「到点」是事件的缺席（§3.3
  否定限制），不可订阅；§8 的 alarm 接口未决，不能依赖。
- 陈旧定时器：打断后旧的完成探针仍会开火——探针读上一帧快照（§2），慢半拍。
  而四层里**没有「撤销一次未来交付」的动词**：唯一动词是 write，写出即事件、
  收不回（D2），注册/注销是注册期概念、帧边界生效（§5）。
- 同帧竞速：完成 op 与打断 op 同帧到达且 D3 无序——「打断抢在完成前」无定义。
- 尾段不可打断：帧戳比较放谓词层（免费、静默）还是 calc 内（可回执）？
- 退款跨帧：扣费在开始帧、退款在打断帧，切错主就有「扣了费没起势」的中间帧；
  法力若在共享池实体，写局部性还禁止直接退。
- 累计受伤 ≥50 是扇入聚合：each 读改写被 D3 推论一明禁；阈值须随每次施法
  清零，而 accum 单写者（D1），别人无法替它重置。

## 切分

核心是**代次守卫**：每次施法发号、任何终结作废现役号；自产 op 携号入场，
settle 只认现役号——不撤销发射，让陈旧交付在消费端失效。全部来源归一化为
同形 op `{target, op, seq, frame, skill, cost}` 合流进唯一结算（手法 7）。

- **entity** `Caster`：`phase`、`cast_seq`、`due_at`（吟唱 = 完成帧；引导 =
  min(下一跳, 终帧)；空闲 = 哨兵 +∞）、`unint_from`、`mana`、`tick_out`
  全归 cast_settle_calc——一 calc 多字段（[05](05-cooldown-state-machine.md)/
  [10](10-damage-pipeline.md) 先例）；法力与施法态同主，退款才能原子
  （[12](12-limited-charges.md) 内聚论证）。`begin_op` 归 begin_calc、
  `cancel_op` 归 cancel_calc、`dmg_accum` 与 `hurt_op` 归 accum_calc、
  `due_op` 归 probe_calc——各 op 字段单写者（D1）。
- 外来晕按协议直接写成同形 op（`seq` 置常量 0）；来源类型膨胀时经
  [06](06-event-materialization.md) 事件实体收敛（[09](09-buff-debuff-ledger.md) 变体同款）。

## 谓词代数

```
# 1 归一化：异形输入各自翻成同形 op（§1.4 出路三）；own 系 op 带 target: self（09 同款）
on    own(cast_req)                 # 05 式带戳请求，冷却守卫同 05，略
each  deliver(new)
→ begin_calc    # 写 own(begin_op) = {target: self, op: "begin", seq: 0, frame, skill, cost}

on    own(move_req)
where cmp(own.phase, ≠, "idle")     # 静止期移动不产 op，守卫免费剪
each  deliver(new)
→ cancel_calc   # 写 own(cancel_op) = {target: self, op: "cancel", seq: own.cast_seq, frame}

# 2 累伤：吟唱期受击 batch 求和入 accum（03 聚合）；accum 随代次自清零
on    type(Attacker, attack_out)
where new.target = self and cmp(own.phase, ≠, "idle")
batch deliver(new.dmg, new.frame)
→ accum_calc    # own(dmg_accum) = {seq, sum}：存号 ≠ own.cast_seq 则从 0 重计
                # sum 跨过 50 → 写 own(hurt_op) = {target: self, op: "interrupt", seq, frame}

# 3 到点探针：完成与分跳共用一根针（01/09 探针：Clock 活阈值盯自己的 due_at）
on    type(Clock, frame)
where cmp(new, ≥, own.due_at)
each  deliver(new)
→ probe_calc    # 写 own(due_op) = {target: self, op: "due", seq: own.cast_seq, frame: new}

# 4 唯一结算：同形合流，多重集内分段裁决（见论证）
on    own(begin_op) | own(cancel_op) | own(hurt_op) | own(due_op) | type(Attacker, stun_out)
where new.target = self
batch deliver(new)
→ cast_settle_calc   # 唯一写 phase/cast_seq/due_at/unint_from/mana/tick_out
```

## 正确性论证

- settle 是 op 多重集上的**分段函数**，每步顺序无关：①代次守卫剪
  `seq ∉ {0, own.cast_seq}` 的自产 op（外来 op 不携陈旧快照，归帧戳管）；
  ②戳守卫剪 `frame ≥ own.unint_from` 的打断类 op（05 同款，戳与阈同源
  Clock、恒偏移封闭）；③有 begin 且 phase = "idle" → 扣费、发新号
  cast_seq+1、due_at/unint_from 落位（begin 走 own 流，写折叠每帧至多一条，
  手法 9，无需决胜）；④due 落账：分跳则写 tick_out、推进 due_at（09 DoT
  同款），到终则完成结算；⑤打断类非空 → 退款 0.7·cost、phase = "idle"、
  cast_seq+1（作废现役号）、due_at = +∞。同帧 due+打断：先④后⑤——
  跳先落账再打断是分段次序（规格自选），不是交付顺序（10 同款重述）。
- 代次守卫不可少，逐帧时间线：帧 B begin 入账（seq=7、due_at=B+90）→
  帧 N=B+90 探针发 due_op{seq:7} → 帧 N+1 settle 完成、seq=8、due_at=+∞，
  但探针本帧读到的 due_at 仍是旧值（快照读）→ 再发 due_op{seq:7} →
  帧 N+2 陈旧 op 因 7≠8 被①剪掉，静默。若玩家恰于 N+2 重新施法（begin 与
  陈旧 op 同批，seq→9），phase 守卫分不清「这是哪一次施法的完成」——新吟唱
  phase 又是 casting，陈旧 op 会让 90 帧吟唱在第 2 帧「完成」；代次一次整数
  比较即分清。重触发只多一帧、幂等消化（09 同款保守性）。
- 同帧竞速：帧 N 探针与晕同写出 → N+1 同批到达，晕的戳 N ≥ unint_from=B+80
  被②剪，完成成立；尾段之外则⑤打断胜。「谁先到」从未被引用，D3 合规。
- accum 顺序无关：批内 Σdmg 可交换（03）；代次比对的两端在一次运行内是
  常量，重置与求和皆多重集函数；跨帧累计依 D1 收拢于 accum_calc 一人，
  「受伤 ≥50」的进度不散落。
- 尾段剪枝位置：②留在 settle 内可写「打断失败」回执；上移为谓词析取守卫
  （`cmp(new.op, =, "due") or cmp(new.frame, <, own.unint_from)`，仍在代数内）
  则零触发但静默——手法 8 的取舍；回执分流见 [11](11-immunity-invincibility.md)。
- 退款原子：扣费与起势、退款与终结各是 settle 同一次运行的多字段写（D1
  允许），不存在中间帧。法力在共享池实体时，写局部性下改写 own(refund_op)
  由池实体记账，跨帧窗口以**帧间 saga**（预留→确认/超时回滚）封口，
  见 [19](19-atomic-spend-trade.md)。
- 引导「保留已结算的跳」白送：tick_out 在过去帧已写出，D2 下没有撤销，
  打断只终止未来。迟到的打断落在 idle 上是空操作，幂等。吟唱中按下的下一个
  技能不打断也不丢——意图寄存，见 [14](14-combo-cancel-buffer.md)。

## 成本

own 链与 move/cast 守卫点查 O(1)（own.phase 仅自己订阅，不退化）；
attack/stun 路由等值桶 O(1)+命中。探针是 `Clock.frame` 活阈值，退化为该
cell 订阅者数 = 全体可施法单位，哨兵 +∞ 也付每帧一次点查（§4 诚实退化
条款）；海量施法者按 [01](01-absence-timeout.md) 翻转为 `Alarm.0`
timer wheel 索引实体，到点集合经 06 事件实体派发。accum/settle 每帧
O(本帧 op 数)；引导每 30 帧一针一跳，成本随跳数计入 |F|。
