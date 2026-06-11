# 10 伤害结算公式：双边管线、护盾与反弹

**语言：** [English](../docs/10-damage-pipeline.md) | 中文

## 问题

「最终伤害 = 攻击 × 倍率 × 暴击 × (1 − 护甲减免) × (1 + 易伤 − 减伤)；
先扣护盾后扣血，真伤穿盾；受击者反弹 10%；同帧多源净算要对，
破盾溢出要精确。」

## 为什么刁钻

- 公式的变量分属两个实例：攻击、倍率、暴击在攻方；护甲、易伤、护盾、血
  在守方。condition 禁 join（§3.3）——没有任何一个地方能「一次读全」公式两半。
- 护盾是**有状态的减免**：同帧 5 发，每发都想问「盾还剩多少」——each 读改写
  禁（D3 推论一），而「第几发破的盾」这种顺序直觉在无序交付下根本不存在。
- 反弹是反馈环：A 打 B、B 弹 A；若 A 也有反弹就是无限镜面。
- 暴击是随机数：随机性放哪儿才不毁回放确定性。

## 切分

**公式按数据主权劈半**：每个因子归属「数据所在的那一侧」。攻侧半
（攻击×倍率×暴击）在攻方 calc 算完、定格成快照进事件载荷；守侧半
（护甲、易伤、护盾、血）由守方 settle_calc 一次净算。两半之间只有值流动，
没有 join。

- **entity** `Attacker`：写 `own(attack_out) =
  {target: ref, raw, tags, reflected: false, frame, salt}`；
  `rng_state` 是 own 字段——随机数状态也是数据，回放确定性白送。
- **entity** `Unit`：settle_calc 唯一写 `hp`、`shield`、`reflect_out`
  （D1 是字段→calc 唯一，一个 calc 持多字段合法，05 先例）；
  易伤/减伤快照读自己的 `buff_book`（[09](09-buff-debuff-ledger.md)，
  同实例跨 calc 读是普通快照读）。

## 谓词代数

```
# 合流：直击与反弹同形（reflected 平铺成标志位——集合成员判定不进谓词层）
on    type(Attacker, attack_out) | type(Unit, reflect_out)
where new.target = self
batch deliver(new, writer_id)
→ settle_calc
# calc 内对多重集 M 净算：
#   eff_i = raw_i × (1 − armor) × (1 + vuln − mit)      ← 守侧半，参数读 own 快照
#   挡序分段：true 伤直达 hp；absorbed = min(own.shield, Σ可挡)，溢出落 hp
#   写 own(shield)、own(hp) 各恰一次；对 reflected = false 的行产出反弹
```

## 正确性论证

- 无 join：condition 封闭集（new、own、常量）足够——因为公式已在数据主权
  边界切开，跨界的只有攻侧半成品快照。推论是**快照语义**：在途伤害用的是
  出手帧的攻击力，落点不重算（落点重算 = 读攻方现值 = join，禁）。
  架构推着规格向「出手定格」收敛——这通常正是业务想要的。
- 「先盾后血」「真伤穿盾」不是顺序，是**多重集上的分段函数**：按 tags 分区、
  区内求和、区间按固定优先级折算。D3 合规；同帧「伤害+治疗+护盾」的结算
  顺序问题整个消失（[03](03-frame-aggregation.md) 同款）。
- 破盾溢出精确：净算下 absorbed 与溢出是确定值；「破盾的那一发」无定义
  （D3）——要发破盾奖励就重述为「本帧致破者中按 (dmg, salt) 取一」
  （[02](02-same-frame-contention.md) 的决胜键纪律）。
- 反弹终止：载荷带 `reflected` 平铺标志，settle 只对 false 行产出反弹 →
  链长 ≤ 2。要做百分比衰减式互弹，用 [08](08-chain-reaction.md) 的
  单调量模板（衰减+下限截断）证终止。
- 反弹扇出撞写折叠：同帧弹给 3 个攻击者是 3 条事件，一个 `reflect_out`
  cell 一帧只能留一条写（§2）——多攻击者时按
  [06](06-event-materialization.md) spawn `ReflectHit` 事件实体，
  settle 合流加一分支（仍同形）。
- 双边因子的归侧：暴击在攻侧 roll、闪避在守侧 roll；命中 vs 闪避这类双边
  判定 = 攻侧把命中数据快照进载荷、守侧补全裁决。每个因子都有唯一归属，
  没有「公式中枢」。
- 落地后的边沿事件（斩杀线、破盾时刻）由下游谓词 `crossed` 盯
  `own(hp)`/`own(shield)` 自取（§7 示例 1），不混进结算。

## 成本

等值 self → 值桶 O(1)+命中；settle 每帧每受击者一次，O(当帧命中数)；
反弹实体 spawn O(1)。全链成本与场上实体总数无关（成本不变量）。

## 变体

**伤害即实体**：把一切伤害统一为 `Hit` 事件实体（06），施加者全部 spawn，
settle 只订阅一个 type——合流分支不再膨胀，回执/格挡协商顺势有了
inst-ref 锚点（手法 3）。代价是每事件一次实例分配；高频弹幕退回
cell 直写 + batch。
