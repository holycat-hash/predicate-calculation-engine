# 23 双人交易 / 对称原子交换

## 问题

「两个玩家面对面交易：各自报价、各自确认，双确认瞬间原子互换。任何一方在
对方确认前**换掉报价**，对方已给的确认必须作废（经典交易诈骗：亮真品，
在对方按确认前一瞬换成赝品）；任何一方取消或中途下线，双方物品分文不差
退回；全程物品不可复制、不可凭空消失。」

## 为什么刁钻

- [19](19-atomic-consume.md) 的 saga 是**不对称**的（买方付钱、商店发货，
  总有一侧先动、一侧裁决）；交易是对称双边：两侧都要押、都要确认，「同时
  成立」在一个没有跨实体原子写的系统里没有原生表达——两侧分两帧应用就有
  可观测的「两边都有 / 两边都没有」中间帧，玩家死亡恰落在缝隙里就是 MMO
  复制 bug 的温床。
- **诈骗窗口本质是 TOCTOU**：确认基于「看到的报价」，报价在确认在途的一帧
  里被换掉，确认到达时核对的是新报价。核对「报价没变」是否定式陈述，
  不可订阅（§3.3）。
- **同帧竞态是最刁的一档**：换报价与对方的确认**同一帧**到达仲裁者，D3 交付
  无序——按到达序处理会出现「确认抢在换价前被接受」的非确定结局。
- 取消、下线、确认、改价五路并发，任意交错都不得停在「一侧已付、一侧未付」。

## 切分

核心手法：**会话实体批内仲裁 + 报价代次失效确认 + 双侧 escrow + 单值判决**。

- **entity** `Trade`（会话实体，[06](06-event-materialization.md) 事件实体化的
  久驻形）：**broker_calc** 单写者持 `offer_a / offer_b / gen / confirm_a /
  confirm_b / state / verdict`。报价与确认全部塌缩进 broker 一次 batch 运行
  ——check 与 act 无帧缝（19 同款）。
- **报价代次**：任何改价 `gen += 1` 并清空双方确认；确认携带它基于的 gen，
  仲裁时 `confirm.gen = gen` 才算数（[18](18-cast-interrupt.md) 施法代次同款：
  陈旧交付消费端失效）。同帧「换价 + 确认」由批内全序解决：报价 rank 先于
  确认 → 换价必然先记账、陈旧确认必然作废——诈骗窗口不是被缩小，
  是被全序**定义掉**了。
- **双侧 escrow**：报价即押（物品 items → escrow，仍留在自己实例里，19 同款），
  改价先退旧押再押新；守恒式在每侧独立成立。
- **单值判决**：双确认成立的那次运行写一条
  `verdict = {commit, to_a: offer_b, to_b: offer_a}`——**两个方向的支付在一个
  值里**。双方钱包经 `inst(pending_trade, verdict)` 同帧收到同一快照，各自
  原子应用自己的半边（清自家 escrow + 收对方半边）；applied_gen 幂等防重放。
- **双向收尸兜底**：玩家死 → runtime 把 `Trade.a/b` ref 写 null（§6.3）→
  broker 判流拍；会话死 → 玩家侧 `pending_trade` 收尸（`became(null)`）→
  reclaim 退押（19 探针同款）。

## 谓词代数

```
# 1 仲裁者：报价 / 确认 / 取消 / 死亡合流；批内全序：报价 < 取消 < 确认
on    type(Player, trade_out) | own(a) | own(b)
where new.trade = self or became(null)
batch deliver(new)
→ broker_calc    # null 行 = 死亡 → abort；offer → 记价、gen += 1、清双确认；
                 # cancel → abort；confirm → gen 相符才记；
                 # 双确认 ∧ gen > 0 → verdict = commit{to_a: offer_b, to_b: offer_a}
                 # state 单调 open → done，done 后短路一切

# 2 钱包：命令、判决、收尸退款合流到唯一写者
on    own(cmd) | inst(pending_trade, verdict) | own(reclaim_op)
batch deliver(new)
→ wallet_calc    # offer：退旧押 → 验足 → 押新 → 发 trade_out；confirm/cancel：转发；
                 # commit：applied_gen 幂等，清自家 escrow + 收对方半边；
                 # abort / reclaim：escrow 全退

# 3 收尸探针：会话死亡 → reclaim（19 同款，幂等吸收假阳性）
on    own(pending_trade)
where became(null)
each
→ reclaim_probe_calc
```

## 正确性论证

- **诈骗关闭**：确认有效性 = 代次等值，是仲裁那次运行内的本地判定。换价与
  确认跨帧到达：gen 已提交，陈旧确认到场即废；**同一帧**到达：批内全序保证
  报价先行 → gen 已变 → 确认作废。不存在「确认基于旧报价被接受」的任何交错
  ——TOCTOU 的 check 与 use 被压进同一次运行（多重集的确定函数，D3 合规）。
- **互换原子**：verdict 是单 cell 单写（一帧至多一条，§2），两个半边在同一个
  值快照里；A、B 经各自的 inst 订阅在同一帧触发、同一帧提交各自半边。帧边界
  上每种物品的全局总量恒定（测试逐帧断言守恒式）。
- **复制关闭**：物品的所有权路径只有 items ↔ escrow ↔（verdict 值交付）→
  对方 items；escrow 的出口仅 commit（清，货已易主）/ abort / reclaim（退），
  无第三种；applied_gen 防重放；state 单调 open → done，done 后 broker 短路
  一切后续 op——双判决不可能。
- **逐崩溃点**（对称 saga）：判定帧前死 → 对应 ref 已 null，broker 收死亡行
  即流拍，双方（幸存者）退押；判定帧死 → 写照常提交、随后结算（§6.3，
  [19](19-atomic-consume.md) 同款论证），verdict 作为值快照照常送达幸存侧，
  死者半边随实例消亡，不产生悬垂也不产生复制；判定后死 → 各侧半边早已各自
  提交完毕。会话实体本身死亡由 pending_trade 收尸兜底，反方向覆盖。
- **活性**：每条路径（成交 / 取消 / 死亡）都把 state 推到 done 且把双方
  pending 归 null；探针的假阳性（正常结清也写 null）被空 escrow 的幂等
  reclaim 吸收——用幂等吸收比区分触发来源便宜（19 同款）。

## 成本

broker 每会话每帧至多运行一次，O(当帧 op 数 · log)；`new.trade = self` 按 §4
应入值桶（脚手架现为线性扇出，TODO 同 19）。交易全程 5 帧（押入 → 记账 →
确认 → 判决 → 应用），是数据流交互的固有代价。与场上实体总数无关
（成本不变量）。

## 可运行验证

`tests/symmetric_trade.rs` 四个用例：双确认同帧原子互换、全程逐帧守恒、判决
后重放陈旧确认无第二次判决；同帧「换价 + 确认」→ 陈旧确认作废、绝不按旧报价
成交，基于新代次重新双确认后按新报价成交（机制不死锁）；中途取消双侧全额
退回；一方死亡 → ref 收尸流拍，幸存方分文不差退款。
