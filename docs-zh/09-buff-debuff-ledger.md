# 09 buff/debuff：叠层、刷新、驱散与面板

## 问题

「攻击 +20% 的强化持续 300 帧，同类重复施加刷新时长、叠层封顶，异类叠乘；
驱散一次移除全部魔法类 debuff；面板攻击 = (基础 + Σ加法) × Π乘法，
任何增删都要实时反映。」

## 为什么刁钻

buff 的直觉定义是「一段时间内持续修改他人属性」，三个词全踩红线：
「他人」撞写局部性（施加者写不了目标的字段）；「持续」撞触发源唯一
（没有免费的每帧 tick）；「修改」撞 D1（基础值与 N 个 buff 都想写 `atk`，
多写者注册期即报错）。再加 D3：同帧两次施加同类 buff 无序到达，
「先施加、后刷新」不可表达。

## 切分

翻转：**buff 不是修改者，是记账凭证**。真状态是目标自己的一本书
（`buff_book`），唯一记账人 book_calc（D1）；面板与书同主、同一次运行写出，
无中间帧。施加、驱散、到期统一为**同形 op 结构**——并（`|`）合流的各分支
必须交付同形负载，单一 condition 才对所有分支同义（[03](03-frame-aggregation.md)
的合流形状纪律，§1.4 单谓词制的代价）；到期这种「异形来源」先由自己的探针
calc 归一化成同形 op 再入流（§1.4 出路三）。

- **entity** `Caster`（任何施加/驱散来源）：写 `own(buff_op_out) =
  {target: ref, op: "apply"|"dispel", kind, add, mul, stacks, dur, tag, salt}`。
- **entity** `Unit`：字段 `buff_book`（Map：kind → {add, mul, stacks, until, tag}）、
  `atk_final` 等面板、`next_expire`、`expiry_op`（自产同形 op）。
- **calculation** `expiry_probe_calc`（挂 Unit）：到期是缺席，按
  [01](01-absence-timeout.md) 由唯一合法轮询者产生正事件。
- **calculation** `book_calc`（挂 Unit）：唯一写书、面板、`next_expire`。

## 谓词代数

```
# 1 到期探针：把「时间到了」翻译成一条同形 op（01 的租约纪律）
on    type(Clock, frame)
where cmp(new, ≥, own.next_expire)
each  deliver(new)
→ expiry_probe_calc    # 写 own(expiry_op) = {target: self, op: "expire", frame: new}

# 2 记账：同形合流；同帧多 op 必须一次收齐（D3 推论一，禁 each 读改写）
on    type(Caster, buff_op_out) | own(expiry_op)
where new.target = self
batch deliver(new)
→ book_calc            # 合并 op 多重集 → 写 own(buff_book)、own(atk_final…)、own(next_expire)
```

## 正确性论证

- D1 红利：「谁在改我的攻击力」收拢为唯一记账人，注册期可审计；施加者永远
  写不到面板——写局部性不是阻碍，恰是 buff 系统需要的隔离。
- D3：book_calc 的合并是 op 多重集上的交换函数——apply 同 kind：stacks 求和
  封顶、until 取 max（刷新）；dispel：按 tag 删组；expire：按 until ≤ frame
  剪枝。任意交付顺序结果相同。同帧 apply+dispel 谁先？没有先——「先并全部
  apply 再应用 dispel」或反之，是**多重集上的分段函数**，规格自选、不是顺序
  （[10](10-damage-pipeline.md) 同款重述）；平局决胜带 salt（02 的决胜键纪律）。
- 重触发窗口：探针帧 N 发 op → N+1 book 剪枝、写新 `next_expire`，但探针在
  N+1 读到的还是旧值（快照读）会再发一次 → N+2 幂等剪枝空操作后安静。
  一帧冗余、幂等消化，01 同款保守性。书空时 `next_expire` 写哨兵 +∞。
- 面板一致性：书与面板同 calc 同帧写出，不存在「书新面板旧」的中间帧；
  下游（UI、[10](10-damage-pipeline.md) 结算读易伤/减伤）订阅或快照读面板即可。
- 帧戳封闭：`until = op.frame + dur`，op 帧戳与探针比较同源 `Clock`，
  偏移恒定不漂移（05 同款）。

## 成本

apply/dispel 路由：`new.target = self` 等值 → 值桶 O(1)+命中。
探针：`Clock.frame` 上活阈值 → 退化为该 cell 订阅者数 = 有 buff 在身的 Unit 数
（§4 诚实退化条款）；海量单位按 01 翻转为 `Alarm.0` timer wheel。
book_calc 每次 O(本帧 op 数 + 在场 buff 数)。

## 变体

- **DoT（中毒每 60 帧一跳）**：书项加 `next_tick`，`next_expire` 推广为
  `next_due = min(全部 until 与 next_tick)`（仍由 book_calc 物化）；due op
  到达时 book_calc 推进 `next_tick` 并写 `own(dot_out)`（与 attack_out 同形），
  汇入 [10](10-damage-pipeline.md) 的伤害合流。
- **行为型 buff**（光环粒子、独立位置）：buff 实体化
  （[06](06-event-materialization.md)），实体只当行为宿主，记账仍走 op→书：
  出生写 apply op，临终帧写 remove op（[08](08-chain-reaction.md) 临终写先例）。
- **施加者类型膨胀**：并的分支随类型增多时，统一经 06 spawn `BuffApply`
  事件实体，scope 收敛为单一 `type(BuffApply, payload)`。
- **光环（范围内持续生效）**：[04](04-dynamic-subscription.md) 的格子进出
  就是 apply/dispel op 的来源。
