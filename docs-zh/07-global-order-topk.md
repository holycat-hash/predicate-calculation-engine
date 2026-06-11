# 07 全局排序 / Top-K 排行榜

**语言：** [English](../docs/07-global-order-topk.md) | 中文

## 问题

「实时维护全服 Top-10 分数榜，UI 在榜单变化时刷新。」

## 为什么刁钻

全局排序天生反稀疏：任何一个分数变化都**可能**影响全序。谓词层故意不收它
（§6.1 点名：join、空间查询、全局排序不进谓词层）——「第 k 名是谁」绑不上
任何逐写 O(1) 的索引（§3.5 准入标准）。但需求是真实的。

## 切分

标准的 §6.1 翻转：**索引即实体，视图即数据**。排序结构是状态，状态属于实例，
维护它的是 calculation。

- **entity** `Board.0`（singleton 索引实体）：字段 `rank_state`（内部有序结构,
  整个结构是一个 Map 值）、`top10`（对外视图，Map："1".."10" → {player: ref, score}）。
- **calculation** `rank_calc`（挂 Board）：增量维护。
- **entity** `Hud` 等消费者：只订阅 `top10`。

## 谓词代数

```
# 收原始写流：batch 整帧一批；deliver 带 old 让更新可逆
on    type(Player, score)
batch deliver(writer_id, new, old)
→ rank_calc        # rank_state 中按 (old→new) 移位；若 top10 变了，写 own(top10)

# 消费视图：变化驱动，不轮询
on    type(Board, top10)
where changed
each  deliver(new)
→ hud_refresh_calc
```

## 正确性论证

- 顺序无关（D3）：batch 的每行携带 `(writer_id, old, new)`，rank_state 的更新是
  「按 id 定位 → 移除 old 位 → 插入 new 位」。单写者+写折叠保证同帧每 Player
  至多一行，行间作用于不相交的键——任意顺序应用结果相同。
- 快照读自洽：rank_calc 读到的 `own.rank_state` 是上一帧值，应用本帧整批增量,
  写回新值——经典的 fold-over-frames，状态机完全显式。
- `top10` 只在真变化时写也行、每帧写也行：D2 写即事件,下游用 `changed` 显式过滤,
  两端解耦。
- 并列分的名次稳定性：决胜键进 rank_state 的排序键（同 [02](02-same-frame-contention.md)，
  不能用 id），保证回放确定性。

## 成本

每条 score 写：路由 O(1)（type+无条件 → 订阅链）+ batch append O(1)；
rank_calc 内部 O(log N) 每行（有序结构）。全帧 O(|W|·log N)，
满足成本不变量形态——而这条最优路径完全在用户层达成，谓词层零扩张。

## 变体

- 全量排序输出（不止 Top-K）：视图字段拆页（`page_0`、`page_1`…），
  消费者按需订阅自己那页——视图即数据，分页也是数据。
- 「我的名次」：Board 写 `top10` 同款思路给每人发名次会撞写局部性——
  改为 Player 自己经 `inst` 订阅 Board 的紧凑视图自查，或名次仅在变化跨过
  关注阈值时经[事件实体化](06-event-materialization.md)广播。
