# 16 经验与掉落的同帧分配

**语言：** [English](../docs/16-xp-loot-split.md) | 中文

## 问题

「怪物死亡：100 经验在 3 个参战成员间整数分配，一点不能丢也不能多；
稀有掉落开一个掷点窗口，窗口内各自 roll、最高者得、迟到 roll 无效；
普通掉落按轮替指针（round-robin）发放，同帧掉 3 件指针要推 3 格；
全程可审计：发出的 = 收到的。」

## 为什么刁钻

- 100 ÷ 3 余 1：浮点均分丢精度，整数均分丢余数。「余数给第一个人」在 D3
  无序交付下「第一个」无定义（[02](02-same-frame-contention.md) 同款）；
  余数的落点必须由业务全序键决定，不能由交付顺序决定。
- 分配者写不了成员的 xp（写局部性）；而一帧要发 n 份，一个 cell 一帧只能
  折叠出一条写（§2）——「逐个发」与「一次广播」都堵死，唯一出口是
  [06](06-event-materialization.md) 的事件实体扇出。
- 掷点窗口的关闭是未来事件 / 缺席（§3.3 否定限制），要靠
  [01](01-absence-timeout.md) 的 alarm；同帧多个 roll 无序齐到，「最高者」
  必须是多重集函数；窗口关闭后的迟到 roll 要在谓词层被挡住，不能进账。
- 轮替指针：同帧 3 件掉落都想 `cursor += 1`——each 读改写明禁（D3 推论一），
  且 cursor 是单 cell，一帧只能落一条写。指针的「同帧推进 3 格」不可能是
  3 次写，只能是一次运行内的纯函数。
- 审计跨实体：Σ发出 与 Σ收到 分属分配者与各成员，没有全局事务可依赖；
  守恒必须由「分账函数恒等」+「每份恰好一次送达」两条独立论证拼出。

## 切分

核心手法：**守恒整除分账（批内全序定余数落点）+ 事件实体扇出 +
窗口守卫仲裁 + 轮替指针批内推进**。

- **entity** `Party`（队伍即分配中枢，单写者记账人）：
  - `roster`（slot → 成员 ref，由 roster_calc 订阅 `type(Member, join_op)`
    维护，[07](07-global-order-topk.md) 的视图即数据）；
  - `rr_cursor` 归 rr_assign_calc；`kill_in` 为伤害/击杀结算的入口
    （上游接 [15](15-summon-attribution.md) 的归因塌缩）。
- **entity** `Award` / `Grant`：[06](06-event-materialization.md) 事件实体，
  载荷 `{target, amount/item}` 出生即定，一帧自决。
- **entity** `Loot`：掷点仲裁者（资源即裁判，02 同款）：`rolls` 归
  collect_calc；`closed`、`winner` 归 award_calc。
- **entity** `Member`：`xp` 归 xp_recv_calc；`bag` 归 item_recv_calc；
  `roll_out` 为掷点出口。

## 谓词代数

```
# 1 整除分账：一次 batch 运行内全序定余数，spawn 扇出
on    own(kill_in)
batch deliver(new)
→ split_calc        # total = Σxp；per = total/n, rem = total%n；
                    # 按 slot 升序前 rem 名 +1（calc 内全序重排，手法 10）；
                    # 逐成员 spawn Award{target: roster[slot], amount}（06）

# 2 领取：恰好一次 = 事件实体出生写一次 + batch 求和
on    type(Award, grant)
where new.target = self
batch deliver(new.amount)
→ xp_recv_calc      # xp += Σ（同帧多份是多重集求和，03 纪律）

# 3 掷点收集：窗口守卫上移谓词层，迟到 roll 静默拒绝（手法 8）
on    type(Member, roll_out)
where new.loot = self and cmp(own.closed, =, false)
batch deliver(new)
→ collect_calc      # rolls[salt] = {member, roll}（键控记账，多重集函数）

# 4 关窗与开奖：alarm 到点（01/§6.2）；winner = max by (roll, salt)
on    type(Clock, alarm)
where new.loot = self
each
→ award_calc        # closed = true；读 own.rolls 取 (roll, salt) 词典序最大；
                    # 写 own(winner) / spawn Grant{target: winner, item}

# 5 轮替分派：同帧 k 件在一次 batch 内按 salt 全序逐件推进，指针一写
on    type(Corpse, drop_out)
where new.party = self
batch deliver(new)
→ rr_assign_calc    # 按 salt 排序；第 i 件给 roster[(cursor+i) mod n]，
                    # 逐件 spawn Grant；写 rr_cursor = cursor + k（一帧一写，§2）
```

## 正确性论证

- **守恒恒等式**：`per×n + rem = total` 是整数恒等式；按 slot 升序前 rem 名
  各 +1，Σ份额 ≡ total，无浮点、无蒸发、无复制。余数落点由 slot 全序决定，
  与交付顺序无关（D3 合规），回放确定。
- **恰好一次**：每个 Award 实体出生恰好产生一条 `grant` 写（06 spawn 即广播）；
  xp_recv 用 batch 把同帧多份当多重集求和——each 读改写的复制风险整个消失
  （[03](03-frame-aggregation.md)）。Award 一帧自决，不积尸。
- **份额落空**：Award 送达前接收者死亡 → 死订阅者被路由跳过，该份 XP 蒸发。
  这是规格分歧点而非 bug：要严格守恒，按 [21](21-linked-life.md) 的
  ack + 收尸回收模板把未确认份额退回 Party 重分。
- **掷点窗口边界**：`own.closed` 是活阈值守卫——关窗后的迟到 roll 免费静默
  拒绝（要回执则拆互补守卫，手法 8）。开奖读 own.rolls 是上一帧快照：与
  alarm **同帧**抵达的 roll 不在快照内——截止语义精确为「alarm 帧之前已写入」，
  是明确的边界而不是竞态。同帧多 roll 由 collect 键控记账、award 在 map 上取
  (roll, salt) 词典序最大——多重集函数，决胜键是业务盐（02 纪律，禁 id）。
- **轮替指针**：cursor 单写者 = rr_assign_calc；同帧 k 件在同一次 batch 运行
  内按 salt 排序后逐件 `(cursor+i) mod n` 分派，cursor 写 `+k` 恰一条
  （写折叠，§2）。「推进 k 格」不是 k 次写，是一次运行内的纯函数；
  下一帧的新掉落看到的已是推进后的指针。
- **审计**：发放侧（split/rr 的台账字段）与接收侧（xp/bag）都是单写者账本
  （[09](09-buff-debuff-ledger.md)）；审计 = 两侧快照求和比对，无需全局锁。
  分账恒等式保证发出守恒，事件实体保证送达恰好一次，两条独立成立。

## 成本

`kill_in`/`drop_out`/`roll_out` 等值 self → 值桶 O(1)+命中；split 每死一次
O(n)；Award/Grant spawn O(份数)；collect O(当帧 roll 数)、开奖 O(参与人数)；
rr O(k log k)。`own.closed` 活阈值仅退化为该 cell 订阅者数（仲裁者自己一行）。
全链与场上实体总数无关（成本不变量）。

## 可运行验证

`tests/xp_loot_split.rs` 三个用例：100/3 分账 34/33/33 且总和守恒；同帧两件
掉落在一次 batch 内轮替分派、指针一次推进两格、各归其主；掷点窗口 alarm 关闭
后迟到 roll 被 `own.closed` 守卫挡住，winner 取 (roll, salt) 词典序最大，
且 alarm 同帧抵达的 roll 不入快照。
