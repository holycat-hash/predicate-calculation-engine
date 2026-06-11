# 13 仇恨与嘲讽表

## 问题

「每个敌人对玩家维护仇恨表：每点伤害 +1 仇恨；治疗为所有与被治疗者交战的
敌人加 0.5×治疗量；嘲讽强制置顶 120 帧；仇恨每秒衰减 5%；切换目标须挑战者
仇恨超过当前目标的 110%（黏滞）；目标死亡或脱战清行重选。」

## 为什么刁钻

- 一张表五路写者：伤害、治疗、嘲讽、衰减、清行都想改 `hate_book`——撞 D1
  注册期报错；同帧伤害+嘲讽+死亡无序到达（D3），「先嘲讽后死」不可表达。
- 治疗是广播：接收方是「所有与被治疗者交战的敌人」——动态集合成员判定，
  condition 封闭集没有（§3.3 禁 join、InSet 只收常量集，
  [11](11-immunity-invincibility.md) 同款刺）；且连「挪进 calc」都得先回答
  「治疗事件凭什么路由到我」。
- 衰减没有事件：「每秒 −5%」直觉是每帧全表打折，在战敌人数 × 表行数的写流
  ——触发源唯一下这是合法但灾难的显式轮询（§6.2 代价自付）。
- 嘲讽改的不是数值是排序；120 帧后的「到期」是缺席（§3.3 否定限制）；
  同帧两个嘲讽谁置顶，D3 下「先到」无定义。
- 110% 黏滞要比较两个**都在衰减中**的量，而比较时刻本身没有事件。

## 切分

三个翻转。其一，**表是账本**：五路异源归一化为同形 op 流，唯一记账人
（[09](09-buff-debuff-ledger.md) 的书/op 纪律，手法 7）。其二，**衰减不写表，
读者折现**——惰性求值（读时折现）：表项存原值、全表共一个戳，真值 =
hate × 0.95^(Δ帧/60) 在读取时刻算出，时间不改数据、改的是解释（同手法的
轨迹形态见 [20](20-projectile.md)）。其三，**嘲讽是词典序覆盖**：置顶键 =
(在嘲讽期?, hate)，「在期内」即 taunt_until 与当前帧比戳，零到期事件（手法 4）。

- **entity** `Player`：`attack_out`（[10](10-damage-pipeline.md) 已有，补
  `kind:"damage"`——同形是设计纪律不是巧合）、`heal_out`、`taunt_out`，统一
  形如 `{kind, target: ref, amount, frame, salt}`，各自产出 calc 写并盖帧戳。
- **entity** `Enemy`：`hate_book`（Map：player_ref → {hate, taunt_until, salt}）、
  `book_stamp`、`current_target`（ref）、`next_due`、`lease_until` 五字段同归
  book_calc——折现、清行、重选必须原子，多字段单 calc 是 D1 下唯一出路
  （[05](05-cooldown-state-machine.md)/10 先例）；`due_op` 归 due_probe_calc，
  `dead_op` 归 dead_relay_calc。

## 谓词代数

```
# 1 记账合流：五路同形 op；治疗分支放行全体，成员过滤下移进 calc（见成本）
on    type(Player, attack_out) | type(Player, heal_out) | type(Player, taunt_out)
      | own(due_op) | own(dead_op)
where cmp(new.target, =, self) or cmp(new.kind, =, "heal")
batch deliver(new, writer_id)
→ book_calc       # 唯一写 hate_book/book_stamp/current_target/next_due/lease_until

# 2 到期探针：嘲讽期满、脱战租约、仇恨跌破阈值，三种「时间到」共用一根（01）
on    type(Clock, frame)
where cmp(new, ≥, own.next_due)
each  deliver(new)
→ due_probe_calc  # 写 own(due_op) = {kind:"due", target: self, frame: new}

# 3 死亡中继：玩家死亡归一化为同形清行 op（§1.4 出路三）
on    type(Player, _alive)
where became(false)
batch deliver(writer_id)
→ dead_relay_calc # 写 own(dead_op) = {kind:"clear_dead", who: 本批集合并, target: self}
```

## 正确性论证

- D3：book_calc 的合并是 op 多重集上的交换函数——damage/heal 按 writer 求和、
  taunt 对 until 取 max、clear_dead 取集合并（dead_relay 同理）、due 幂等；
  「先入账还是先清行」是多重集上的分段函数，规格自选、不是顺序（09/10 同款
  重述）。同帧双嘲讽 until 相同：置顶取 (until, salt) 词典序 max，salt 为
  业务盐、禁 id（[02](02-same-frame-contention.md) 决胜键）。
- 保序定理：全表同戳 ⇒ 任意读取帧的真值 = 原值 × 共同正因子 ⇒ 表内排序与
  110% 比较**对原值直接成立，一次折现都不用**；红利只属于同速率指数衰减，
  异速（嘲讽仇恨衰减更快之类）就必须逐行折现到共同帧。绝对阈值（脱战 ε）
  才要折现值，而衰减是确定函数：跌破帧 = 戳 + log(ε/h_max)/log(0.95^(1/60))
  解析可得，并进 next_due，探针准点只醒一次；跨表比较（UI、转火）两戳不同，
  各自折现到共同帧再比。
- 黏滞不抖：current_target 单写者 + batch 一帧一运行 + 写折叠 ⇒ 每帧至多一次
  换目标。跨帧：A 顶 B 需 A > 1.1B，B 反顶需 B > 1.1A，合取要求 B > 1.21B，
  无新 op 不可能；衰减保比例且不产生任何 run——黏滞条件在两次 op 之间根本
  不被重估。嘲讽在词典序高位，越过 110% 强制置顶；期满由探针唤醒重选，
  回落到最高仇恨者。
- 时间线：帧 N 玩家 A 写 attack_out{damage,12}、牧师 H 写 heal_out{heal,8}
  （戳 N−1：写者快照读 Clock，恒偏移同源封闭，05 模板）→ 帧 N+1 E.book_calc
  一批收齐：折现全表换新戳、A 行 +12；heal 过滤读的是上帧 hate_book，A 尚不
  在表，本条不入账——「交战」自伤害入账帧起算，保守一帧（01 同款）；要含
  同帧改为先并本批 damage 再滤 heal，仍是分段函数 → 帧 N+2 坦克 T 写
  taunt_out → 帧 N+3 T 行 until=戳+120、current_target→T、next_due←min(各
  until, lease_until, 跌破帧) → 帧 M ≥ next_due 探针发 due_op → 帧 M+1 重选/
  清行、写新 next_due；探针 M+1 仍读旧值再发一次，M+2 幂等空操作（09 冗余帧）。
- 死亡与脱战：dead_op 清任意行（不只当前目标），清行与重选同 calc 同帧原子；
  窗口期内 current_target 由 runtime 写 null（§6.3），不悬垂。脱战 = 租约
  （手法 5）：交战 op 入账即续 lease_until，到期清全表、目标写 null、next_due
  写哨兵 +∞。重选 = 表多重集上 (在嘲讽期, hate, salt) 词典序 max——顺序无关
  函数，空表给 null。

## 成本

伤害/嘲讽路由：`new.target = self` 等值 → 值桶 O(1)+命中。治疗分支是诚实退化
剖面：`kind = "heal"` 常量桶里站着全体挂此谓词的敌人，每条治疗 O(在册敌人数)
触发 + calc 内过滤，|F| 被不相关敌人灌水（§4）。敌人多而交战局部时翻转
（手法 6 + §6.1）：交战关系物化为 `Encounter` 实体——heal_out 带 encounter
ref，Encounter 以 `new.encounter = self` 等值收治疗、写 own(heal_log)，成员
经 inst(encounter_ref, heal_log) 订阅、归一化入流（手法 7）；每条治疗降为
O(本场敌人数)，代价是成员维护与一帧中继。衰减零每帧成本：无 tick 无 run，
折现 pow 由读者付，book_calc 每次 O(本帧 op 数 + 表行数)。探针：`Clock.frame`
活阈值退化为在战敌人数（§4 诚实退化条款）；海量按 [01](01-absence-timeout.md)
翻转 Alarm.0。群嘲一帧只折叠一条写（§2），要嘲讽多个敌人按
[06](06-event-materialization.md) 事件实体化，出生即广播。
