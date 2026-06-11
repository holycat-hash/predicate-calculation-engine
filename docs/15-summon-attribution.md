# 15 召唤物归属与击杀归因

## 问题

「玩家召唤宝宝、宝宝再放图腾，任何后代的击杀都记到根主人：击杀者得经验、
最大贡献者得称号进度、伤害 ≥10% 者进助攻名单；宠物可驯服转让；
主人退场后留下孤儿宠物；在途火球命中时主人已转让——归谁。」

## 为什么刁钻

- 「记到根主人」的直觉是沿 owner 链向上查根——每跳都在读别人的行，
  多级 join，condition 封闭集（§3.3）一跳都不给。
- 「最后一击」在 D3 无序交付下无定义：致死帧多发齐到，「最后」不存在。
- 受益人都不是死者：死者写不了别人的 XP（写局部性）；助攻名单是变长集合，
  接收侧「self ∈ new.assists」不可表达——字段路径静态，InSet 只收常量集（§3.5）。
- 转让与死亡都在改链：在途火球按落点查归属还是 join；主人销毁后
  owner ref 被 runtime 写 null（§6.3），孤儿挂在断链上。

## 切分

**查链翻转为平铺**：根不查，随身带。spawn 后代时初始字段 root_owner =
自己的 root_owner（根单位 = self；runtime 代写初始字段，06 先例）——查根永远
是读 own 字段。转让 = 根重写 + 同形镜像逐层下传；归因记在受害者侧，记账即塌缩。

- **entity** `Unit`：`owner`、`root_owner`、`grand_owner`（ref）与镜像源
  `lineage_pub` 四字段唯一归 lineage_calc（一 calc 多字段，05/10 先例）；
  `tame_out`（驯服者的 tame_calc 写）、`orphan_op`（orphan_calc 写）为同形谱系 op；
  `attack_out`（attack_calc 写，载荷定格 attacker_root，[10](10-damage-pipeline.md)）；
  `hp`、`damage_book`（root → Σdmg，[09](09-buff-debuff-ledger.md) 书形）归
  settle_calc；`xp`、`title_progress`、`assist_log` 归 credit_calc。
- **entity** `KillCredit`（事件实体，[06](06-event-materialization.md)）：
  `grant = {beneficiary: ref, kind, amount}`，spawn 时 runtime 代写，阅后即焚。

## 谓词代数

```
# 1 谱系：驯服/镜像/孤儿同形合流（手法 7）；op = {target, op, owner, root, grand, salt}
#   裸 root_owner 与 op 异形 → 父辈在谱系真变时同帧续发同形 lineage_pub（target: null）
on    type(Unit, tame_out) | inst(owner, lineage_pub) | own(orphan_op)
where cmp(new.target, =, self) or cmp(new.target, =, null)
batch deliver(new)
→ lineage_calc   # op 多重集：retame 压过 mirror（分段函数），平局按 salt（02）；
                 # 写 owner/root_owner/grand_owner，真变再续 lineage_pub

# 2 结算+记账：受害者侧一次净算（10）；书键 = 载荷 attacker_root，记账即塌缩
on    type(Unit, attack_out)
where new.target = self
batch deliver(new)
→ settle_calc    # 并书 book[root] += Σdmg（旧书快照+本帧多重集，12 的余额纪律）；
                 # 净 hp ≤ 0：killer = 致死帧多重集 max by (dmg, salt)，top/助攻
                 # 出自书 → 逐受益人 spawn KillCredit；写 own(_alive) = false

# 3 学分：受益人等值认领（06）；同帧多份必须 batch 一次收齐（D3 推论一）
on    type(KillCredit, grant)
where new.beneficiary = self
batch deliver(new)
→ credit_calc    # 按 kind 累进 own(xp) / own(title_progress) / own(assist_log)

# 4 孤儿：主人销毁 → runtime 写 owner = null（§6.3）
on    own(owner)
where became(null)
each
→ orphan_calc    # 自决 destroy_self()，或写 own(orphan_op)：野化 root = self；
                 # 继承 owner = own.grand_owner，root 不变（中间层死不动根）
```

## 正确性论证

- 无 join：root_owner 只在 spawn 初始写与 op 载荷里流动，全部来自写入方
  own 快照；condition 只用 new 与常量（self/null）——封闭集够用。
- D1 与 D2：谱系四字段收拢为唯一记账人 lineage_calc；镜像与孤儿不是第二写者，
  是归一化成同形 op 的第二来源（§1.4 出路三，09 同款）。pub 续传以真变为闸
  （D2：变没变要显式问），子树底部安静，环状驯服一圈收敛即停。
- 转让与在途同一口径：帧 N 写 tame_out → N+1 宠物重写根、发 pub → N+2 一级
  后代 → N+3 二级……每层一帧；窗口内后代的 attack_out 与在途火球一样，带的
  都是出手帧定格的 attacker_root（10 出手定格，[20](20-projectile.md) 同款）——
  **快照语义，记旧主**。落点重查 = 读攻方现值 = join，禁，且攻方彼时可能已
  销毁（ref 已 null）：架构推着规格向出手定格收敛，传播窗口无需特例。
- 记账即塌缩：书若记直接攻击者，死亡帧要把每人解析到根——又是多级 join 且
  攻击者可能先死；键取载荷快照，归因与 hp/致死判定同主同帧（12 的内聚论证）。
- 「最后一击」重述为致死帧多重集内 max by (dmg, salt)（02 决胜键，10 的破盾
  先例；salt 是业务字段，不用 id）。并书是按键求和，killer/top/assists 全是
  多重集与书的函数——谱系、结算、认领三处 batch 均顺序无关，D3 合规。
- 学分两条路：A）death_report 平铺单受益人字段 + `new.killer = self` 等值认领，
  零分配，但变长助攻名单表达不了；B）逐受益人 spawn KillCredit 等值认领。
  取 B 统一三种角色（kind 区分）；只剩击杀者的规格可退回 A。
- 逐帧：帧 N 攻击 → N+1 settle 并书、判死、spawn 学分、写 `_alive = false`
  （帧边界结算，指向死者的 ref 置 null）→ N+2 受益人 batch 认领；死者宠物
  owner became(null) 同帧触发 → N+3 孤儿策略经 orphan_op 落账。
- 孤儿继承：目标 = 主人的主人，死人行读不得——靠镜像 op 顺路平铺的 grand_owner
  （grand = 父辈 owner）。继承后新 grand 暂缺待新主下次发布，两代同帧阵亡按
  野化兜底——要么预先平铺要么放弃。根销毁时反向表一次性把全子树 root_owner
  写 null（§6.3 是平的，不逐层级联），后代凭 became(null) 转野。

## 成本

三处认领均为等值 → 值桶 O(1)+命中（§4）；谓词 1 的 or 注册期拆双等值桶（§5
归一化），代价不变；inst 哈希链 O(1)。settle 每受击者每帧一次 O(命中数)，死亡帧
加 O(书大小)；转让传播合计 O(子树大小) 条写、O(深度) 帧时延——只与真实后代数
有关，与全局规模无关（成本不变量）。退路：割草场景 spawn 数 = O(受益人总数)，
代价敏感且规格只剩击杀者时退回路 A（零分配）；书膨胀按 09 加 next_expire 探针
剪枝——探针是 Clock.frame 活阈值，退化为带书单位数（§4 诚实退化条款），海量
时翻转为 Alarm 索引实体。
