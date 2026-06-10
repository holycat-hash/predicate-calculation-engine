# 02 同帧多方抢唯一资源

## 问题

「N 个 Unit 同帧申请拾取同一个 Item，恰好一个成功，其余各方都要知道结果。」

## 为什么刁钻

- 写局部性：没人能直接写 `Item.owner` 之外实例的字段，胜负无法「通知」。
- D3：申请以 batch 到达且**顺序未定义**，「先到先得」不可表达——裁决必须是
  申请多重集上的顺序无关函数。
- id 无顺序语义（§1.2），不能拿 id 当决胜键。

## 切分

- **entity** `Unit`：字段 `claim`（ref+优先级的结构）、`want`（ref，指向想抢的 Item，
  作为回执通道的订阅锚点）。
- **entity** `Item`：字段 `owner`（ref）。Item 自己就是仲裁者——资源即裁判，
  不需要第三方实体。
- **calculation** `claim_calc`(挂 Unit)：写 `own(claim) = {item: ref, prio: p, salt: s}`
  同时写 `own(want) = item_ref`。
- **calculation** `grant_calc`(挂 Item)：收整帧申请，裁决，写 `own(owner)`。
- **calculation** `on_result_calc`(挂 Unit)：经 inst-ref 盯 `owner` 收回执。

## 谓词代数

```
# 仲裁：batch 收一帧内全部申请
on    type(Unit, claim)
where new.item = self and not cmp(own.owner, ≠, null)   # 已有主则不再裁
batch deliver(writer_id, new.prio, new.salt)
→ grant_calc      # winner = max by (prio, salt)；写 own(owner) = winner_ref

# 回执：申请方经自己持有的 ref 盯结果（inst scope 是「别人行」的唯一合法读法）
on    inst(want, owner)
each  deliver(new)
→ on_result_calc  # new = self → 成功；否则失败，清 own(want)、另寻目标
```

## 正确性论证

- 顺序无关：`max by (prio, salt)` 是多重集函数，与交付顺序无关（D3 合规）。
  决胜键 `salt` 是业务字段（随机盐或入场序），不依赖 id 顺序。
- 平局：(prio, salt) 仍并列时裁决不确定——这是规格问题不是机制问题；
  要求确定性回放就保证决胜键全序。
- D1：`owner` 唯一归 grant_calc；申请方永远写不到它。
- 时序：帧 N 申请 → 帧 N+1 裁决写 owner → 帧 N+2 各申请方收回执。
  两帧延迟是数据流交互的固有代价（§7 示例 2 同款）。
- 守卫 `owner = null`：占用后的迟到申请不再触发仲裁；失败方经回执自行放弃。

## 成本

申请路由：`new.item = self` 等值条件 → 值桶 O(1)+命中数（§4）。
batch append O(1)/条；裁决 O(当帧申请数)；回执 inst 订阅 O(1)+触发数。
