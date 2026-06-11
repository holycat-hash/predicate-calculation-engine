# 19 原子消耗与双花

## 问题

「一把钥匙，同帧两扇门都想消耗——只能开一扇；商店购买：扣金币在玩家、
扣库存在商店，任意一步失败或对方中途消失，都不得丢钱、丢货或复制货。
经典 MMO 物品复制 bug 不允许出现。」

## 为什么刁钻

- **check-then-act 在快照读下天然竞态**：帧 N 两个 calculation 都读到
  `keys = 1`，各自判「够」，帧 N+1 各扣一次——双花。这不是实现 bug，是快照
  语义（§2）的必然：读到的是上一帧，判定与提交之间永远隔着一帧，任何分散的
  「先查再用」都基于过期数据。经典物品复制 bug 的根源正是「多个判定者各持
  过期快照」。
- D1 恰好是解药的一半：余额单写者 → 没有第二个 calc 能扣钱；但单写者自己
  也会在同一帧收到两个请求——原子性必须在「单写者一次 batch 运行内的多重集
  仲裁」里完成（[02](02-same-frame-contention.md)），且 D3 下「先到先得」
  不可表达，要业务全序键。
- **跨实体没有原子写**：扣钱（Player）和扣货（Shop）各自帧内原子，但二者
  相隔两帧，中间任何一帧对方都可能拒绝或死亡，停在中间态。这正是分布式
  saga 的形状：预留（escrow）→ 确认 / 补偿，必须逐帧枚举崩溃点逐个论证。
- 「超时回滚」是缺席事件（§3.3 否定限制）：要靠 §6.3 的 ref 收尸
  （`became(null)`）或 [01](01-absence-timeout.md) 租约；而回滚还可能与
  迟到的确认赛跑——丢钱与复制货只隔一个交错。

## 切分

核心手法：**单写者批内仲裁关双花 + escrow 帧间 saga + req 台账幂等 +
ref 收尸触发回滚**。

- **entity** `Player`：`balance`、`escrow`（req → 押金台账）、`reserve_out`、
  `pending_shop`（ref，收尸锚点）、`spend_log` 同归 **wallet_calc**
  （一 calc 持多字段，05/10 先例——余额与押金间的搬运必须在一次运行内原子）；
  `items` 归 stash_calc；`reclaim_op` 归 reclaim_probe_calc。
- **entity** `Shop`：`stock`、`decide_out` 归 shop_calc（库存的判定与扣减
  原子）；多买家同帧的回执扇出按 [06](06-event-materialization.md) spawn
  回执实体（`decide_out` 一帧只能一条写，§2）。
- 异源归一（手法 7）：spend / buy / grant / reject / reclaim 全部归一为
  同形 op `{target, kind, …, req|salt}`，合流进唯一 wallet。

## 谓词代数

```
# 1 钱包：一切动钱的 op 合流到唯一写者，批内全序仲裁
on    own(buy_req) | type(Door, demand) | type(Shop, decide_out) | own(reclaim_op)
where new.target = self
batch deliver(new)
→ wallet_calc   # 按 (kind 优先级, salt) 重排（手法 10）：先 grant/reject
                # （结清旧约）再 reclaim（收尸退款）后 spend/buy（花新钱）；
                # spend：够则扣、不够记拒；
                # buy：balance → escrow[req]，写 reserve_out 与 pending_shop=shop；
                # grant：删 escrow[req]（钱真正花掉）；
                # reject：删 escrow[req] 并退回 balance；
                # reclaim：全部 escrow 退回 balance

# 2 商店：库存的 check 与 act 同处一次 batch 运行，无帧缝
on    type(Player, reserve_out)
where new.shop = self
batch deliver(new)
→ shop_calc     # 按 req 全序逐单：stock > 0 → stock -= 1、decide_out = grant；
                # 否则 decide_out = reject。同帧多单的竞争塌缩进一次运行

# 3 入库：只认 grant，过滤在谓词层
on    type(Shop, decide_out)
where new.target = self and cmp(new.kind, =, "grant")
batch deliver(new)
→ stash_calc    # items += 批大小

# 4 收尸触发回滚：§6.3 死亡结算把 pending_shop 写成 null
on    own(pending_shop)
where became(null)
each
→ reclaim_probe_calc    # 写 own(reclaim_op) = {target: self, kind: "reclaim"}
```

## 正确性论证

- **双花关闭**：check 与 act 之间没有帧缝——都在 wallet 一次 batch 运行内：
  对请求多重集按全序逐单试扣，扣谁不扣谁是多重集的确定函数（D3 合规，
  02 决胜键纪律）。两扇门同帧抢一把钥匙 → 恰一个 ok 一个 no。复制 bug 的
  根源被 D1 连根拔掉：判定者只有一个，且**它判定的那次运行就是提交**。
- **逐帧崩溃点**（saga 核心论证）：
  - 帧 N（预留提交后）：balance 减、escrow 增，守恒式
    `balance + Σescrow + price×items` 不变。玩家「少了钱」但钱在自己的
    escrow 里——没有跨实体在途资金这种状态。
  - 帧 N+1（商店判定）：stock 扣减与 grant 写出在同一 calc 的写集里一并提交
    （§2），不存在「扣了库存没发货」的可观测中间帧。商店在判定**前**死 →
    reserve 路由落空（死订阅者被跳过），无任何状态变化；在判定**帧**死 →
    写照常提交、随后才结算（§6.3），grant 作为值快照照常送达、不悬垂
    （[08](08-chain-reaction.md) 同款）。
  - 帧 N+2（玩家结清）：grant → 删台账（钱花定）；reject → 退款。两者都
    幂等：台账无此 req 即无操作。
  - 商店死亡：runtime 写 `pending_shop = null`（§6.3）→ reclaim → escrow
    全退。**迟到 grant 与 reclaim 赛跑？** grant 存在 ⇒ 商店活到判定帧 ⇒
    判定先于死亡结算 ⇒ grant 不晚于 reclaim 到达；同帧时 wallet 批内全序
    先 grant 后 reclaim——台账已删，reclaim 无可退。任何交错下守恒式不破。
  - wallet 自己写 `pending_shop = null`（正常结清）同样会触发 reclaim_probe
    ——空台账上的 reclaim 是无操作。用幂等吸收假阳性，比区分触发来源便宜。
- **货不复制**：stock 单写者批内扣；grant 每 req 恰一次（decide_out 一帧
  一写 + req 台账去重）；items 只认 `kind = grant`。**货不丢**：reject 与
  收尸必退款——escrow 没有第三种出口。
- **锁的影子**：escrow 就是「带业务语义的锁」，但它是数据不是机制——可审计、
  可回放；死锁不存在，因为没有等待原语，只有帧推进与到点回收。

## 成本

全部等值 self → 值桶 O(1)+命中；wallet / shop 每帧每实例至多运行一次，
O(当帧 op 数 · log)；saga 全程三帧延迟是数据流交互的固有代价（02 同款）。
与场上实体总数无关（成本不变量）。

## 可运行验证

`tests/atomic_consume.rs` 四个用例：同帧双花恰一成功（批内全序仲裁）；
购买 saga 全程逐帧验证守恒式；库存不足 reject 全额退款；预留后商店死亡，
`became(null)` 收尸触发回滚，资金守恒、无复制。
