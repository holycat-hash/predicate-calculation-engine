# 17 位移与击退的同帧合成

## 问题

「同一帧：两个爆炸的击退、自己的冲刺、脚下减速带、所站平台的载体移动同时作用；
定身禁主动位移但强制位移仍生效，霸体抵抗击退；两个钩子同帧钩同一目标恰一个赢；
合成结果不得穿墙。」

## 为什么刁钻

- 爆炸、冲刺、输入、平台各是一套系统，都想写 `position`——D1 多写者注册期即报错。
  「上物理引擎求合力」也不是出路：业务规则不是向量和（钩子赢者通吃、定身只禁一类）。
- 同帧诸位移无序到达（D3）：「先击退再冲刺」「后钩顶掉前钩」不可表达；
  两钩恰一个赢且败者要知情——写局部性下没人能去「通知」它。
- 站平台 = 订阅目标随所站载体运行期变化，而谓词注册期定型（§0）；平台还会死。
- 定身/霸体是「按位移类别说不」：逐 calc 防御 if 既晚（|F| 预算已花）又散，守卫该上移（05/11）。
- 不穿墙取决于全图几何——condition 禁 join（§3.3），墙塞不进谓词。

## 切分

- **entity** `Bomb` / `Hook`（一切位移施加者）：写 `own(kb_out)` / `own(hook_out)`，负载归一为
  同形 op `{target: ref, class, kind, vec, prio, frame, salt}`（手法 7 形状纪律）；`Hook` 另持
  `hook_target`（ref 回执锚点，02 的 want 同款），钩头飞行与命中见 [20](20-projectile.md)。
  单发多目标撞写折叠（§2，一 cell 一帧一条写）→ 按 [06](06-event-materialization.md)
  spawn `KnockHit` 事件实体，合流加一分支仍同形。
- **entity** `Unit`：`position`、`grab_winner` 同归 **mover_calc**（一 calc 持多字段，05/10
  先例：裁决与落点原子写出）；`dash_op`/`move_op` 归 dash_calc / intent_calc——技能位移与
  输入意图各自归一成 op（带 `target: self`，09 的 expiry_op 同款）走同一条流；`carry_op` 归
  carry_calc；`platform`（ref）归 board_calc；`root_until`、`kb_resist`、`slow_mul` 由
  [09](09-buff-debuff-ledger.md) 的 book_calc 物化——定身/霸体/减速带都是 buff（减速带经
  [04](04-dynamic-subscription.md) 格子进出产生 apply/dispel），守卫参数全是 own 字段（手法 4）。
- **entity** `Platform`：position 由其驾驶 calc 写；`Grid.0` 物化「格 → 载体」占位视图与
  实心格视图（§6.1，手法 1）。

## 谓词代数

```
# 1 位移合流：一切来源归一为同形 op 入流；定身守卫只挡主动类（11 的分类守卫）
on    type(Bomb, kb_out) | type(Hook, hook_out)
      | own(dash_op) | own(move_op) | own(carry_op)
where new.target = self
      and not (cmp(new.class, =, "active") and cmp(new.frame, <, own.root_until))
batch deliver(new)
→ mover_calc
# calc 内对 op 多重集 M 分段净算（10「多重集上的分段函数」的位移版）：
#   ① 钩子排他：{kind = "hook"} ≠ ∅ → 按 (prio, salt) 取一，位移 = 其 vec，
#      其余强制位移作废；写 own(grab_winner) = winner_ref
#   ② 否则强制类 ≠ ∅ → Σ 击退向量 × (1 − own.kb_resist)（霸体 resist = 1 → 零向量）
#   ③ 否则主动类 ≠ ∅ → 取最大模 by (|vec|, salt)
#   ④ 否则常规类 → Σ 输入向量 × own.slow_mul（乘性减速是 own 参数轨，加性冲量是 op 轨）
#   载体轨恒叠加：+ Σ class = "carrier" 向量（相对运动，不参与类间覆盖）
#   碰撞：快照读 Grid.0 实心视图，沿合成向量截到首个碰撞面；写 own(position) 恰一次

# 2 钩子回执：被钩者即裁判，败者经 inst-ref 得知落空（02 仲裁，手法 3）
on    inst(hook_target, grab_winner)
each  deliver(new)
→ hook_result_calc     # new = self → 命中收线；否则清 own(hook_target) 另寻目标

# 3 载体相对运动：订阅目标随 platform 现值走（04 重定向）
on    inst(platform, position)
where changed
each  deliver(new, old)
→ carry_calc           # 写 own(carry_op) = {target: self, class: "carrier", vec: new − old, …}

# 4 换乘：落点重定位脚下载体 = ref 重写（04）
on    own(position)
where changed
each  deliver(new)
→ board_calc           # 快照读 Grid.0 载体占位视图 → 写 own(platform)；无载体写 null
```

## 正确性论证

- **D1 收敛**：position 唯一归 mover_calc；任何新位移来源（传送、风场）只能发 op 入流，
  注册期强制——「谁在动我」可审计（09 的 D1 红利同款）。grab_winner 与 position 同帧
  原子写出，不存在「裁了没动」的中间帧。
- **batch 顺序无关（D3）**：按 class/kind 字段值划分多重集与顺序无关；类间覆盖只取决于
  「类非空」这个多重集谓词；类内 Σ 可交换，max by (|vec|, salt) 与 (prio, salt) 取一是
  多重集 max——决胜键禁 id，prio/salt 是业务字段，并列时裁决不确定，决胜键全序是规格
  责任（02 同款）。一个要写明的规格自由度：覆盖判定用「类非空」而非「合成非零」——
  霸体把击退衰减成零向量时是否仍冻结主动类，两种选择都是多重集函数，任选其一。
- **守卫与衰减的分工**：定身挡主动类在路由层（不触发不交付，静默拒绝最便宜，手法 8）；
  霸体衰减必须在 calc——被衰减的击退仍要参与「类非空」判定，剪在路由层会改掉分段语义。
  `root_until` 与 op.frame 同源 `Clock` 恒偏移，比较封闭（05 论证模板）。
- **逐帧时间线**：帧 N 两 Bomb 写 kb_out、两 Hook 写 hook_out、intent_calc 写 move_op
  （持住摇杆 = 驱动侧逐帧写；D2 写即事件，不需要 changed）→ 帧 N+1 合流收齐一个多重集，
  mover_calc 取一钩、截碰撞、写 position 与 grab_winner → 帧 N+2 败者钩经谓词 2 收落空
  回执；board_calc 重定位脚下载体；落点进新格引发的连锁（地刺、感应）沿 [04] 的
  locate→occupancy→react 与 [08](08-chain-reaction.md) 的帧间 ping-pong 逐帧展开，本帧不重入 mover。
- **载体滞后诚实账**：平台帧 N 动 → N+1 carry_op → N+2 单位动，两帧固有（§7 示例 2）；
  渲染侧读 platform 现值把单位挂进载体坐标系可消视觉滞后。平台销毁：runtime 写
  `platform = null` 并解除 inst 订阅（§6.3），null 本身就是「不在载体上」的正确终态，零额外谓词。
- **快照读封闭**：墙是上一帧的实心视图——同帧新落的墙下一帧才挡，一帧误差帧间收敛；
  静止单位无 op 写即无触发，position 不被无谓重写。

## 成本

合流路由：`new.target = self` 等值 → 值桶 O(1)+命中；own 分支哈希链 O(1)；命中后 root
守卫为活阈值点查 O(1)（§4 诚实退化条款：上界为该 cell 订阅者数，已被值桶先剪，11 同款）。
mover 每帧每单位至多一次（batch），O(本帧 op 数 + 轨迹跨格数)；inst 回执与载体链 O(1)+触发数。
退化与翻转：持续移动 = 每单位每帧一条 move_op，|W| 自付且显式（§6.2 同理）；风场逐帧给
场内 N 单位发 op 是 O(N)/帧——翻转为 09 光环入书、恒定外力物化成 own 参数轨（slow_mul
同款），仅进出场时有 op；AOE 多目标经 06 付 spawn O(1)/事件，高频弹幕退回单目标 cell 直写（10 变体同款权衡）。
