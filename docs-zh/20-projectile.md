# 20 投射物：飞行、穿透与命中去重

**语言：** [English](../docs/20-projectile.md) | 中文

## 问题

「箭每帧飞 v，命中即停；穿透箭至多穿 3 个目标且同一目标只结算一次；高速箭
一帧越过整个格子不得隧穿；飞行中被风场偏转、被盾反弹后归属反转；射手中途
死亡箭照飞、命中照常归因。」

## 为什么刁钻

- 自驱动：投射物自己要动，没有外部触发源。唯一合法出口是订阅 `Clock.frame`
  （§6.2）——合法且诚实，但每帧 O(弹数) 条 position 写灌满 |W|；触发源唯一，
  这笔账偷不掉，只能换记账方式。
- 隧穿：逐帧采样是点模型，速度 > 格宽/帧就从格间漏过——不是数值 bug，
  是 [04](04-dynamic-subscription.md) 点式占用订阅对线段语义的失配。
- 穿透预算：「沿途先命中前 3 个」是全序语句，D3 下候选交付无序，「第 n 个」
  无定义（[12](12-limited-charges.md)）；each 读改写扣预算被 D3 推论一明禁。
- 归属：落点读射手现值是 join（§3.3 禁）；ref 名牌在 §6.3 被写 null，
  不定格则命中时无处可读；可一旦定格，盾反之后又该归谁？
- 命中即停：停止决定与同帧其余候选竞速——停了还结算？还是漏掉该结算的？

## 切分

核心翻转是**惰性求值（读时折现/轨迹参数化）**：发射时写一次
`traj = {origin, v, t0}`，位置成为时间的纯函数 pos(t)，**不逐帧写 position**；
命中判定移交索引实体做线段扫掠。少量慢弹保留逐帧自推的诚实基线，按弹种分层。

- **entity** `Projectile`：`traj`、`cred`（owner ref＋队伍/加成**平铺**快照，
  [15](15-summon-attribution.md) 口径）同归 steer_calc——偏转/盾反须原子重写
  两者，一 calc 持多字段是 D1 下唯一出路（05/10 先例）；发射初值随 spawn 写入
  （06 出生即写）。`pierce_left`、`hit_set`（Map：目标→已结算）、`_alive`
  归 settle_calc。
- **entity** `Grid.0`（[04](04-dynamic-subscription.md) 索引实体扩两块）：
  `traj_table` 归 track_calc；`cursor`（每弹已扫游标）归 sweep_calc，
  后者读 traj_table 是同实例快照读（10 先例）。
- **entity** `HitCand`、`Hit`：事件实体（[06](06-event-materialization.md)），
  载荷出生即定、一帧自决；受击侧复用 [10](10-damage-pipeline.md) 管线。

## 谓词代数

```
# 0 诚实基线（少量慢弹）：§6.2 显式轮询自推，真动真付；海量弹幕走 1–4
on    type(Clock, frame)
each
→ fly_calc      # 写 own(position)；占用与命中走 04 的点式订阅

# 1 轨迹入表：traj 写 = 换段；_alive=false = 删行收殡（并分支同投影）
on    type(Projectile, traj) | type(Projectile, _alive)
batch deliver(writer_id, new)
→ track_calc    # 写 own(traj_table)[writer_id]：键控覆盖/删行，多重集函数

# 2 扫掠：Grid.0 独占轮询（集中化，01 的 Alarm 同款）；交线段不交采样点
on    type(Clock, frame)
each
→ sweep_calc    # 每段求 [max(cursor, t0), now]×格集之交，对照占用快照产候选：
                # spawn HitCand{proj: ref, target: ref, t, salt}（06）；写 own(cursor)

# 3 弹侧裁决：穿透预算与同目标去重的唯一裁判
on    type(HitCand, hit)
where new.proj = self
batch deliver(new)
→ settle_calc   # 多重集内：剔除 own.hit_set 已结算目标 → 按 (t, salt) 全序重排
                # （手法 10 的几何版）→ 取前 own.pierce_left 个 → spawn
                # Hit{target, dmg, cred 平铺, frame, salt}（受击侧并入 10 的合流）
                # 写 own(hit_set)、own(pierce_left)；预算尽或非穿透 → own(_alive)=false

# 4 偏转与盾反：异形先归一化成同形 op 再合流（手法 7）
on    type(Wind, deflect_op) | type(Shield, parry_op)
where new.proj = self
batch deliver(new)
→ steer_calc    # op 同形 {proj, v', cred'|null}：风场只改 v'；盾反 v' 反向且
                # cred' = 盾主出手定格快照——重新出手，后续命中归新主（15 口径）
                # 同帧多 op 按 (优先级, salt) 取一（17 的合成纪律）；原子重写 traj＋cred
```

## 正确性论证

- 时间线：帧 N 发射（traj/cred 定格）→ N+1 入表 → N+2 首扫 [t0, N+2]（游标自
  t0 起，**注册延迟不漏段**）、spawn 候选 → N+3 settle 裁决、spawn Hit、自决
  → N+4 受击者结算。四帧全是真实数据依赖（04 同款）；表现层用 pos(t) 读时插值。
- 恰穿 3：预算判定必经 batch 一次运行的多重集（12 纪律）。「沿途先后」D3 无定义，
  但轨迹参数 t 白送业务全序键——按 (t, salt) 重排取前缀是手法 10 的合法确定性
  恢复；同 t 由 salt 决胜（02 决胜键，禁 id）。同目标去重 = hit_set 单写者记账。
- 命中即停不重结算：本帧候选收敛于同一 settle 多重集，取前 k 其余**当场弃置**；
  _alive=false 帧边界解除订阅（§6.3），扫掠多跑的迟到候选路由不到任何谓词，
  免费蒸发——无跨帧泄漏。HitCand 一帧自决，不积尸。
- 偏转语义：traj 是唯一事实，pos(t) 由旧段定义直到新段写入生效——两帧延迟是
  §7 示例 2 的固有代价，不是误差。缝合帧的重叠候选：同目标 hit_set 兜底；
  严格口径给候选带段代次、settle 弃旧代——代次守卫（[18](18-cast-interrupt.md)）。
- 盾反 ≠ [10](10-damage-pipeline.md) 的 reflected 标志：后者防镜面递归（终止），
  此处重写 cred 解决归属（语义）；cred 带 bounce_count 上限截断，
  按 [08](08-chain-reaction.md) 单调量模板证终止。
- 射手死亡：cred 在发射帧平铺定格（10 出手定格）；§6.3 只把 ref 值写 null，
  平铺学分是值不是引用——箭照飞、归因照常，快照语义白送。
- 顺序无关核对：track 键控覆盖（写折叠保证每弹每帧至多一条 traj，§2）、settle
  去重＋排序＋前缀、steer 优先级合成——三个 batch 消费者皆多重集函数，D3 合规。

## 成本

发射 O(1) 写对基线每帧 O(弹数) 写：|W| 从弹数级降到改写数级，路由费（§4 按
|W| 计）整笔省掉。`Clock.frame` 两订阅者条件均不含 own 字段，无活阈值退化。
扫掠是 sweep_calc 内紧凑循环 O(活段数＋Σ交格数)——真实工作真动真付，赖不到
调度头上；游标语义天然支持分摊补扫（每帧只扫一个分片，欠账下帧补齐不漏段），
这是惰性求值的红利。HitCand/Hit spawn O(候选数)；settle O(k log k)，k 为当帧
候选数。全链与场上弹总数、实体总数无关（成本不变量）；慢弹走基线、快弹走
参数化的分层共存是诚实的混合解，不必全场一刀切。

## 变体

抛物线/追踪弹：pos(t) 换任意纯函数族；追踪 = `inst(target, position)` 触发
重参数化（04 重定向），目标死亡 `became(null)` 收尸转直飞。激光/即时命中：
段长一帧打满的单次扫掠，轨迹退化为纯查询，traj_table 都不必驻留。
