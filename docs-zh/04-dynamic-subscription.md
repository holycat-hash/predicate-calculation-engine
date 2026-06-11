# 04 动态订阅目标与空间邻域

## 问题

「近战 Unit 只关心自己所在格子的占用变化；Unit 会移动换格。」

## 为什么刁钻

谓词是注册期定型的数据（§0）：scope 里的字段静态、类型静态，**不能运行期换订阅目标**。
「我关心的格子」恰恰是运行期变量。把全图占用塞进 `Grid.0` 的一个大字段再订阅
`changed` 是 O(全图) 的假稀疏；逐格开字段then「订阅第 (x,y) 个字段」又回到动态订阅。

## 切分

钥匙是 §3.2 本来就给的：`inst(ref, field)` 的**订阅目标随 ref 字段的值走**。
ref 是 cell，cell 可以被自己的 calculation 改写——**改写 ref = 重定向订阅**，
零注册操作，完全在数据流内。

- **entity** `Cell`（每格一实例，地图加载时 spawn）：字段 `occupants`（Map）。
- **entity** `Grid.0`（singleton 索引实体，§6.1）：字段 `cell_table`
  （Map："x,y" → Cell ref，建图时写定，之后只读）。
- **entity** `Unit`：字段 `position`、`my_cell`（ref，指向所在 Cell）。
- **calculation** `locate_calc`（挂 Unit）：position 变了 → 快照读 `Grid.0.cell_table`
  查到新格的 ref → 写 `own(my_cell)`。（读不是依赖：读谁都行，触发必须靠订阅。）
- **calculation** `occupancy_calc`（挂 Cell）：维护本格占用。
- **calculation** `react_calc`（挂 Unit）：对所在格占用变化做出反应。

## 谓词代数

```
# 1 换格：position → my_cell（ref 重定向）
on    own(position)
where changed
each  deliver(new)
→ locate_calc          # 写 own(my_cell) = Grid.0.cell_table[格(new)]

# 2 占用维护：进格 new = self，出格 old = self；batch 无序无碍（集合维护顺序无关）
on    type(Unit, my_cell)
where cmp(new, =, self) or cmp(old, =, self)
batch deliver(writer_id, new, old)
→ occupancy_calc       # 写 own(occupants)：加 new=self 的，删 old=self 的

# 3 邻域感知：订阅目标 = my_cell 现值，换格自动改盯
on    inst(my_cell, occupants)
each  deliver(new)
→ react_calc
```

## 正确性论证

- 谓词 3 注册期只定型「经 my_cell 字段、盯 occupants 字段」这个**形状**；
  指向哪个实例由 runtime 的 ref 反向表逐帧维护——动态性放在数据里，不在谓词里。
- 谓词 2 的占用维护是集合加删，多重集函数，D3 合规。
- 一次移动的传播：帧 N 写 position → N+1 locate 写 my_cell → N+2 occupancy 写 occupants
  → N+3 同格者 react。每帧延迟都是一次真实的数据依赖，无隐藏耦合。
- Cell 销毁时 runtime 把所有 `my_cell` 写成 null（§6.3），持有者 `became(null)` 收尸,
  不会盯死格。

## 成本

谓词 2 是 `= self` 等值 → 值桶 O(1)+命中（§4）；谓词 3 是 inst 哈希链 O(1)。
对比不切分的做法（每 Unit 每帧扫邻域）：O(N×M) → O(移动写数)。
范围查询（半径 r 内全部敌人）同款翻转：Grid.0/Cell 把「查询结果」物化成字段，
查询者订阅结果字段——**索引即实体，视图即数据**（§6.1）。
