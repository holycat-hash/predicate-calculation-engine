# 03 帧内聚合：多攻击者合伤、多来源合流

**语言：** [English](../docs/03-frame-aggregation.md) | 中文

## 问题

「同一帧 5 个 Attacker 打同一个 Unit，外加 2 个 Healer 治疗，结算后的 hp 要正确。」

## 为什么刁钻

直觉写法是沿用 §7 示例 2 的 each：每条命中 `hp = own.hp - dmg`。错。
D3 推论一（§2）：each 下同一 calculation 一帧运行多次，**各次基于同一快照**——
5 次运行都读到 `hp = 100`，各写 `100 - dmg_i`，写折叠顺序未定义，最终值随机取一。
**帧内聚合一律用 batch 或 fold，禁止用 each 做读-改-写累加。**

## 切分

- **entity** `Attacker`：写 `own(attack_out) = {target: ref, dmg: 5}`。
- **entity** `Healer`：写 `own(heal_out) = {target: ref, amount: 3}`，结构同形
  （`amount` 也叫 `dmg`、取负，则两路完全同构）。
- **calculation** `settle_hp_calc`（挂 Unit）：唯一写 `hp` 的人（D1），
  一帧收齐全部增减量，求和后**写一次**。

## 谓词代数

```
# 伤害与治疗合流：scope 并（|）= 任一来源写入即命中；batch 整帧聚一批
on    type(Attacker, attack_out) | type(Healer, heal_out)
where new.target = self
batch deliver(new.dmg)              # 治疗交付负值
→ settle_hp_calc                    # hp' = clamp(own.hp - Σ rows)；写 own(hp) 恰一次
```

## 正确性论证

- 求和对多重集封闭：Σ 与交付顺序无关（D3 合规）。
- 同帧伤害+治疗的「结算顺序」问题消失了：没有顺序，只有一帧的净增量。
  若业务要求「先伤后疗、死了就不能奶」这种**帧内序**，那不是聚合问题，
  是状态机问题——把死亡判定放进同一个 settle_hp_calc（它看得到净值与净前值），
  或拆成跨帧两段（见 [08](08-chain-reaction.md) 的帧间展开）。
- 为什么不用 `fold sum`：fold 聚合的是**被订阅 cell 值的 ±delta**（§3.4），
  适合「全场 Enemy hp 总和」这类对**字段现值**的聚合；
  而这里要的是「本帧发给我的事件载荷之和」，按订阅者分组（`target = self`），
  载荷在结构体里——batch + calc 内求和才是对位的工具。
- clamp、暴击、护甲等任意逻辑都在 calculation 里——图灵完备性留在该在的层。

## 成本

`new.target = self` 等值条件 → 值桶 O(1)+命中数；batch append O(1)/条（§4）。
settle 每帧每受击者运行一次，O(当帧命中数)。
