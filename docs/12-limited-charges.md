# 12 仅生效 N 次：亡语、护符层数、第 N 次强化

## 问题

「亡语恰好触发一次；护符抵挡 3 次致命伤，第 4 次起失效；每第 5 次攻击附带
强化。一帧内 5 发同时砸到只剩 1 层的护符——恰好挡 1 发，不能挡 5 发
也不能挡 0 发。」

## 为什么刁钻

「恰好 N 次」是计数不变量，天敌是**同帧多发 × 快照读**：守卫读的是上一帧
计数，同帧 K 发全部看见「还有余额」。each 交付下 calc 跑 K 次、各自基于
同一快照——效果重复发出，计数写折叠取尾，「N」被踩穿（D3 推论一明令禁止
的 each 读改写，正是此处）。边沿守卫（became/crossed）只挡跨帧重入
（[01](01-absence-timeout.md)/[05](05-cooldown-state-machine.md) 的用法），
挡不住同帧。「第 5 次」更直接蕴含全序第 n 个——D3 无序，「第几发」不存在。

## 切分

先认清一条白送的定理，再给三件套：

**own 流恰好一次定理**：D1 单写者 + 写折叠（§2）⇒ 任一 cell 每帧至多一条写
⇒ own scope 的谓词每帧至多触发一次 ⇒ **own 流上「边沿条件 + each」就是
exactly-once**，零额外机制。同帧多发只存在于扇入流（type/inst 收别人的写）。

**扇入流三件套**：单写者计数字段 + batch（一帧收敛为一次运行）+
多重集内裁决。

- **entity** `Unit`：`hp`、`charge`（护符层数）、`hit_count`、`empower_next`
  全归 settle_calc——致命判定要看净 hp，扣层与扣血必须同主同帧
  （内聚不是偷懒，是 D1 的要求）。
- **calculation** `deathrattle_calc`（挂 Unit）：盯 own(hp) 边沿。

## 谓词代数

```
# 1 亡语：own 流 + 边沿 = 恰好一次（settle 是 hp 唯一写者，每帧至多一写；
#   护符在 settle 内把致命帧定格在 hp = 1，穿越只发生在层数耗尽之后）
on    own(hp)
where crossed(0, ↓)
each
→ deathrattle_calc     # 写 own(rattle_out)；destroy_self()

# 2 限次抵挡 + 第 N 次强化：与伤害结算同一条谓词（合流分支按 10 扩）
on    type(Attacker, attack_out)
where new.target = self
batch deliver(new)
→ settle_calc
# calc 内：net = Σ 守侧减免后伤害（10）
#   若 own.hp − net ≤ 0 且 own.charge > 0：
#     写 own(charge) = own.charge − 1；写 own(hp) = 1；写 own(ward_fx_out)
#   hit_count += |本批|；跨过 5 的倍数 → 写 own(empower_next) = true
#   （模运算进 calculation——谓词层无模，§3.5）
```

## 正确性论证

- 不变量归纳：`charge` 单写者，每帧消耗 ≤ 快照值、写回差值——「总消耗 = N」
  逐帧归纳成立。batch 保证一帧恰一次运行，「同帧 5 发抢 1 层」在一次运行的
  多重集内裁决，恰挡其一。
- each+守卫为什么不行：守卫挡跨帧、漏同帧——它给出的是「每帧至多 K 次」，
  不是「至多一次」。**exactly-N 必经 batch**。
- 层=帧 还是 层=发：上面给的是「一层保一帧净击」。要「逐发结算」（一层挡
  一发，第二发照死）则在 calc 内**按全序键 (dmg, salt) 重排后顺序模拟**——
  D3 只是不承诺交付序，calc 对多重集自施全序是合法的确定性恢复手段
  （[07](07-global-order-topk.md) 的 rank_state 同理：输出仍是输入多重集的
  函数）。两种语义都 D3 合规，规格自选。
- 「第 5 次」的合法投影（任选其一，写进规格）：**帧粒度跨越**——本帧
  hit_count 跨过 5 的倍数，强化本帧按决胜键选出的一发；或**寄存**——写
  `empower_next`，下一帧首批生效（自写自读，快照天然隔帧）。
  「全局第 n 发」本身在无序世界无定义，规格必须二选一。
- 亡语不重入：crossed 是边沿；实例随即自决，cell 不再有写
  （[08](08-chain-reaction.md) 同款双保险）。
- 共享池（全队共 3 次复活）：跨实例限次 = 同帧多方抢唯一资源，整套换
  [02](02-same-frame-contention.md) 的仲裁+回执，计数归池实体单写。

## 成本

own 流谓词 O(1) 哈希链；扇入流等值桶 O(1)+命中；settle O(命中数)
（逐发语义加 O(k log k) 帧内排序，k 为当帧命中数）。与全局规模无关。
