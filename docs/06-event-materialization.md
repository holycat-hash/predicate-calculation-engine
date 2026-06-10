# 06 事件实体化：一帧多事件、一事件多接收者

## 问题

「撮合器一帧撮合出多对交易，每对的双方都要收到通知。」
（同构问题：一帧多次爆炸、批量到期的定时器、一次掉落多件战利品。）

## 为什么刁钻

两堵墙同时挡路：

1. **写折叠（§2）**：一个 cell 一帧只能留下一条写。撮合器把 10 对结果先后写进
   `own(match_result)`，折叠后只剩最后一对——cell 是状态通道，不是队列。
2. **condition 封闭集（§3.3）**：就算把 10 对塞进一个 Map 值，接收者也写不出
   「self ∈ new 的某处」——字段路径是静态的，不能按运行期 id 索值。

## 切分

事件不是值，是**出生**。spawn 一个实体承载一条事件：每条事件独立的 cell 组，
写折叠失效；接收者用普通等值条件认领自己那份，封闭集够用。

- **entity** `Matcher.0`（singleton）：batch 收申请（仲裁同 [02](02-same-frame-contention.md)）。
- **entity** `Trade`（事件实体）：字段 `members`（{a: ref, b: ref, price}）、`ttl_seen`。
- **calculation** `match_calc`（挂 Matcher）：每撮合出一对就
  `spawn(Trade, members = {a, b, price})`——一帧 spawn 任意多个，互不折叠。
- **calculation** `on_trade_calc`（挂 Unit）：认领与自己相关的 Trade。
- **calculation** `reap_calc`（挂 Trade）：事件实体阅后即焚。

## 谓词代数

```
# 认领：spawn 时 runtime 代写初始字段（§6.3），members 的写入就是广播
on    type(Trade, members)
where new.a = self or new.b = self
each  deliver(new, writer_id)        # writer_id = Trade 实例 ref，可作回执锚点
→ on_trade_calc

# 阅后即焚：出生后第二帧自决（留一帧给认领路由）
on    own(_alive)
where became(true)
each
→ mark_calc            # 写 own(ttl_seen) = true

on    own(ttl_seen)
where became(true)
each
→ reap_calc            # destroy_self()
```

## 正确性论证

- 每条事件一个实例 → 没有共享 cell → 写折叠不再吞事件；D3 无序无碍，事件间本就独立。
- 认领条件是普通等值（`new.a = self`），走值桶索引，不需要任何新谓词原语——
  这正是 §3.5 准入标准的反向应用：表达力不够时翻数据，不翻词汇表。
- 生命期阶梯：帧 N spawn（runtime 写 `_alive`、`members`）→ N+1 双方认领触发、
  mark_calc 触发 → N+2 reap 自决 → N+3 帧边界结算，指向它的 ref 被写 null（§6.3）。
  认领发生在 reap 之前，不丢事件。
- 需要回执/握手的事件：双方把 `writer_id` 存进自己的 ref 字段，经
  `inst(trade_ref, …)` 继续后续协商（手法 3），Trade 实体顺势成为协商状态机的宿主，
  寿命改由协商完成事件驱动而非 TTL。

## 成本

spawn 是 O(1) 分配 + k 条初始写；认领等值条件 O(1)+命中。
事件实体的代价是实例分配/回收的常数——换来的是队列语义在四层内的合法表达。
代价敏感的高频小事件（每帧成千上万）退回 batch 聚合（[03](03-frame-aggregation.md)），
让单一接收者一次吃整批。
