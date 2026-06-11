# 08 连锁反应：爆炸链、传染、多米诺

**语言：** [English](../docs/08-chain-reaction.md) | 中文

## 问题

「炸药桶被引爆，波及邻近的桶，邻桶再爆——链式展开直到烧完。」

## 为什么刁钻

连锁是天然的**反馈环**：爆炸 → 伤害 → 爆炸。事件总线架构里这是重入/递归深度/
帧内无限循环的事故多发地。PCE 里它反而是白送的：双缓冲把反馈环天然展开为
帧间 ping-pong（§2），**不存在帧内循环**。刁钻点只剩两个：邻域怎么查（空间），
以及怎么证明链会停。

## 切分

复用前篇积木，零新概念：

- **entity** `Barrel`：字段 `hp`、`my_cell`（ref，[04](04-dynamic-subscription.md) 的格子模式）、
  `explosion_out`。
- **calculation** `explode_calc`（挂 Barrel）：hp 跌穿 0 → 写
  `own(explosion_out) = {cell: my_cell_ref, dmg: 50}` 并 `destroy_self()`。
- **calculation** `splash_calc`（挂 Cell，[04](04-dynamic-subscription.md) 的格子实体）：
  收爆炸，把波及量写成本格字段。
- **calculation** `take_splash_calc`（挂 Barrel）：经 `inst(my_cell, splash)` 收伤害。

## 谓词代数

```
# 起爆：边沿触发，一个桶只爆一次（crossed 不会重复，实例随即销毁）
on    own(hp)
where crossed(0, ↓)
each
→ explode_calc        # 写 own(explosion_out)；destroy_self()

# 波及落格：格子聚合本帧落在自己身上的爆炸（batch：同帧多爆求和，D3 合规）
on    type(Barrel, explosion_out)
where new.cell = self                   # 简化：仅本格；邻格见下方变体
batch deliver(new.dmg)
→ splash_calc         # 写 own(splash) = {dmg: Σ, seq: own.seq + 1}（seq 保证 D2 事件性）

# 受波及：动态盯自己所在格
on    inst(my_cell, splash)
each  deliver(new.dmg)
→ take_splash_calc    # 写 own(hp) = own.hp - new.dmg   （单爆源；多源同帧合并见 03）
```

## 正确性论证

- 展开节奏：帧 N 桶 A hp 跌穿 → N+1 explosion_out 落格 → N+2 邻桶扣 hp →
  N+3 邻桶 crossed 起爆……每跳一帧,链长 = 传播半径，调试时逐帧可视。
- 不重爆：`crossed(0, ↓)` 是边沿不是水平（§7 示例 1 同款）；且爆桶已自决,
  其 cell 不再有写。死亡结算把指向它的 ref 写 null（§6.3），同帧在途的爆炸
  交付的是**值快照**，不会悬垂。
- **终止性**：系统总 hp 单调不增、每次起爆严格减少活桶数，且爆炸只在
  「hp 从 ≥0 到 <0」的边沿产生——链必然在有限帧内停止。论证模板：
  找一个被链严格消耗的单调量（能量/活实例数/未感染数）。
- 同帧多爆波及同格：splash_calc 是 batch 求和，[03](03-frame-aggregation.md) 的纪律；
  多格爆源打同一个桶则按 03 把 take_splash 改 batch。

## 变体

- **邻格波及**：爆炸结构带半径，splash 谓词改为 Cell 订阅
  `type(Barrel, explosion_out)` 无 self 等值条件时会全格扇出——正确做法是
  爆桶在 explosion_out 里列出受波及格 ref 不可行（cond 不能按运行期 ref 集过滤），
  应由 Grid.0 物化「爆炸 → 受波及格集合」视图，逐格写字段（索引即实体，§6.1）。
- **传染模型**（感染概率、潜伏期）：潜伏 = [05](05-cooldown-state-machine.md) 时间戳守卫；
  概率与免疫判定进 calculation。

## 成本

链的每一跳：等值/inst 路由 O(1)+命中数。整链总成本 = O(被波及实体数)——
与场上桶总数无关，符合成本不变量。
