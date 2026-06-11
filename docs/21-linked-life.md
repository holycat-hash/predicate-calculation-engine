# 21 链接生命与伤害转移

## 问题

「保镖技能：B 替 A 承受 50% 伤害；A、B 互相保护会不会无限转发；
转移来的伤害还能不能再转移；总伤害必须不多不少恰好等于原始伤害；
保镖在转发途中死了，那一半伤害去哪。」

## 为什么刁钻

- **镜面环**：A→B→A→… 双缓冲保证无帧内循环（§2），每跳占一帧；但帧间会
  不会永远弹？[10](10-damage-pipeline.md) 的 reflected 标志（链长 ≤ 2）在
  守恒口径下是错的——它把「转移一次后就不再转移」当公理，比例语义丢失。
  需要 [08](08-chain-reaction.md) 单调量模板的**守恒版**：量不消失，只沉淀。
- **守恒分账**：50% 要取整。`⌊25×0.5⌋ = 12`，剩下的 13 给谁？浮点会让总
  伤害凭空蒸发或凭空创造；余数必须有确定落点（[16](16-xp-loot-split.md)
  的余数纪律）。
- 转发是跨实体：转发者写不了保镖的 hp（写局部性）；伤害在途的那一帧，
  「我已扣的」与「对方将扣的」之间没有任何全局账本可以对账。
- **在途落空**：保镖死了，转发事件的目标实例已不存在——死订阅者被路由跳过，
  那 50 点伤害静默蒸发，立刻变成「杀掉自己保镖免伤」的 exploit。而「对方
  没收到」是缺席（§3.3 否定限制），不可订阅——要 ack 正事件与 §6.3 收尸
  两面夹。
- 直击、转发、回执、回收四路异源都要动 `hp` 与在途台账——D1 下只能同形
  归一合流进唯一结算者（手法 7；[13](13-aggro-taunt.md)/[18](18-cast-interrupt.md) 同款）。

## 切分

核心手法：**整数衰减单调量证终止 + 余数留存守恒分账 + ack/收尸双面兜底**。

- **entity** `Unit`：
  - `hp`、`fwd_out`、`ack_out`、`pending`（在途台账 salt → 金额）、`fwd_seq`
    同归 **settle_calc**（一 calc 持多字段：扣血、转发、记账必须在一次运行
    内原子，05/10 先例）；
  - `guard`（ref）+ `ratio` 归 link_calc（谁建立保镖链谁写）；
  - `reclaim_op` 归 reclaim_probe_calc。
- 同形 op `{target, kind, amount?, salt, source?}`：
  `hit`（直击，[10](10-damage-pipeline.md) 管线出口）/ `fwd`（转发）/
  `ack`（回执）/ `reclaim`（收尸回收）。

## 谓词代数

```
# 1 唯一结算：四路同形合流，批内全序（ack → reclaim → 伤害）
on    own(hit_in) | type(Unit, fwd_out) | type(Unit, ack_out) | own(reclaim_op)
where new.target = self
batch deliver(new)
→ settle_calc   # ack：pending.remove(salt)
                # reclaim：hp -= Σpending；pending = {}（在途全部落回自己）
                # hit/fwd 净算（10）：D = Σamount；有保镖 → f = ⌊D×ratio⌋，
                #   keep = D − f（余数留本地，守恒）；f > 0 → 写 fwd_out
                #   {target: guard, amount: f, salt: seq++, source: self}，
                #   pending[salt] = f
                # 对收到的每条 fwd 回写 ack_out{target: source, salt}
                # hp -= keep

# 2 保镖收尸：§6.3 把 guard ref 写成 null → 在途与未来的转发全部回头
on    own(guard)
where became(null)
each
→ reclaim_probe_calc    # 写 own(reclaim_op) = {target: self, kind: "reclaim"}
```

## 正确性论证

- **终止性（镜面环）**：转发量 `f = ⌊D×r⌋`，r < 1 且 D、f 为整数 ⇒
  对一切 D ≥ 1 有 f < D 严格成立，D = 1 时 f = 0 链止。单调量 = 在途转发量：
  每跳严格递减、有下界 0——这是 08 模板的守恒版：能量不是被丢弃截断，而是
  逐跳沉淀为各单位的 hp 扣减。链长 ≤ ⌈log₁⁄ᵣ(D₀)⌉ 帧，100 点 50% 七帧收敛。
  r ≥ 1 应在注册期拒绝——那不是镜面环问题，是规格写了永动机。
- **守恒分账**：`keep + f = D` 是整数恒等式，floor 的余数留在转发者本地
  （16 的余数纪律：不丢、不复制、落点确定）。对链归纳：每跳把 D 拆成
  keep（立即沉淀）+ f（继续在途），Σ(全链 hp 扣减) = D₀，无浮点。
- **在途落空两面夹**：正面 ack——保镖活着结算必回 ack，转发者删台账；
  反面收尸——保镖死则 runtime 写 `guard = null`（§6.3），reclaim 把 pending
  全额落回自己 hp。时序枚举：
  - 保镖死于转发送达**前** → 路由跳过死订阅者，fwd 蒸发，但 pending 还在，
    reclaim 兜回；
  - 保镖死于结算**同帧** → 它的写照常提交（值快照不悬垂，08），ack 与
    `guard = null` 同帧到达转发者，批内全序先 ack 后 reclaim：台账已删，
    reclaim 落空为无操作——不会扣两次。
  任何交错下总扣血 = D₀；「杀保镖免伤」的 exploit 不存在。
- **转移的伤害再转移**：fwd 与 hit 同形进同一净算，保镖对转来的伤害照样按
  自己的 guard 再拆——语义是递归的，机制只是同一条谓词。不允许二次转移就
  给 op 加 hops 平铺位（10 的 reflected 同款），但守恒要求 hops 截断时
  keep 全额，而不是丢弃。
- **同帧多源**：同帧被直击 30 + 转入 25 → 一次 batch 净算 D = 55 再拆，
  「先直击后转发」的顺序问题消失（[03](03-frame-aggregation.md)/10）；
  ack / reclaim / 伤害的批内顺序由 calc 内全序固定（手法 10），与交付序无关。
- **扇出上限**：`fwd_out`/`ack_out` 一帧一写（§2）。净算后转发天然单条；
  但同帧收到多个 source 的 fwd 要回多条 ack——撞写折叠 → 按
  [06](06-event-materialization.md) spawn Ack 事件实体，合流分支不变。
  丢 ack 不破守恒：ack 只清台账，兜底由 reclaim 完成；代价是活保镖期间
  台账可能滞留，审计口径应把 pending 计为「已判定未确认」。

## 成本

全部等值 self → 值桶 O(1)+命中；settle 每帧每单位至多一次 O(当帧 op 数)；
链每跳一帧，总成本 O(链长) = O(log D₀)，与场上单位总数无关（成本不变量）。

## 可运行验证

`tests/linked_life.rs` 三个用例：A→B→C 链 50/25/25 守恒分账；A、B 互保
镜面环在有限帧收敛、总扣血恰 100、在途台账清空；保镖死亡时在途转发经
`became(null)` 收尸全额回落，伤害不蒸发。
