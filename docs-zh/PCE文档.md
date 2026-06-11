# PredicateCalculationEngine

**语言：** [English](../docs/PCE.md) | 中文

---

## 0. 总则

本架构有且只有四层抽象：**runtime、entity、calculation、predicate**。任何新需求必须折叠进这四层，禁止引入第五种概念。本文档后续出现的一切机制——时钟、生命周期、空间索引、跨实体交互——都是这四层的推论，不是扩展。

系统是纯数据驱动的：唯一的触发源是「上一帧的写入（write）」。不存在轮询、消息、事件总线；它们全部被统一为「对某个 cell 的 write」。**cell** 指 entity 实例的一个字段，是数据、写入与订阅的最小粒度。

predicate 层是封闭代数：谓词是注册期定型的声明式结构（数据 / AST），不是任意函数。图灵完备性全部留给 calculation。这一限制不是风格偏好，而是性能保证的来源（§4、§3.5）。

### 0.1 决议记录

**D1 单写者制** —— 每个字段静态归属唯一一个 calculation；归属冲突在注册期报错。

**D2 写即事件** —— write 一律产生事件，无论值是否变化；「值真的变了」由条件 `changed` 显式表达。

**D3 batch 不排序** —— batch 的交付顺序未定义，runtime 可按任意顺序（分片序、到达序）交付，路由阶段不做任何排序或定序屏障。

此外，§1.4 的「单谓词制」与 §2 的「快照读、写折叠」是为闭合语义、由 D1–D3 与写局部性推导固定的条款，属本稿新钉死的内容，验收时请重点复核。

---

## 1. 四层抽象

### 1.1 runtime

唯一的调度者与索引持有者。职责：维护数据双缓冲；收集帧 N 的写集；在帧 N+1 将写集路由给谓词；维护谓词索引与 fold 增量状态；管理实例生命周期、id 分配与 ref 反向表（§6.3）；并作为系统内建的 writer 向内建 cell 写入（如 `Clock.frame`，§6.2）。

runtime 不承载任何业务逻辑。它对谓词的全部「理解」来自注册期编译（§5）。

### 1.2 entity

实例化的最小单位，写作 `entityname.id`。id 无顺序语义、可复用；复用安全性由 runtime 内部代际号保证（§6.3），对用户不可见。所有数据属于某个 entity 实例，不存在游离于实例之外的全局数据——全局性状态以 **singleton entity**（单实例类型）表达，如 `Grid.0`、`Clock.0`。

entity 本身没有行为；行为全部在挂于其类型之下的 calculation。

### 1.3 calculation

挂在 entity 类型下、predicate 之后。输入是前置 predicate 的交付（**值的快照，不是引用**）；输出是对**自己实例字段**的 write。

**写局部性** —— calculation 只能写自己实例的字段。跨实例影响只能经由数据流：写自己的字段，等待被对方的 predicate 嗅探（典型模式见 §7 示例 2「攻击」）。

**单写者制（D1）** —— 字段静态归属唯一 calculation，注册期检查，冲突即错。由此 `new` 的定义无歧义，并行执行无需仲裁。

**快照读** —— calculation 读任何字段（包括自己的）读到的都是上一帧已提交的值；本帧写入对本帧一律不可见（§2）。

calculation 内部是任意图灵完备代码。

### 1.4 predicate

挂在 calculation 之前，形状为三段式 `(scope, condition, delivery)`，详见 §3。

**单谓词制** —— 一个 calculation 恰有一个前置 predicate。理由：若允许多个不同 delivery 形态的谓词喂同一个 calculation，其输入签名将不再单一。组合需求在四层内已有出路：「任一来源」用 scope 的并（`|`），「同帧共现」用合取（`&`），「触发流 + 聚合量」则把聚合先物化——同一 entity 上另设一个 fold 谓词 + calculation 把聚合值写成字段，主 calculation 经 `project(own.f)` 读取（见 §3.4）。

---

## 2. 帧模型与数据流

```
帧 N:    calculation 执行,write 进入帧 N 写缓冲
帧 N+1:  阶段一(路由)  runtime 持帧 N 写集 W:
                       索引查找 → 条件判定 → 填充 each 触发 / batch 缓冲 / 更新 fold
         阶段二(执行)  被触发的 calculation 运行,write 进入帧 N+1 写缓冲
```

**双缓冲推论。** 所有谓词在帧 N+1 看到帧 N 的一致快照；任何 cell 的 `old` 值免费可得；反馈环（A 触发 B、B 触发 A）天然展开为帧间 ping-pong，不存在帧内循环。

**并行性。** 路由阶段按 cell 分片并行；执行阶段按 entity 实例并行。由写局部性与单写者制，执行阶段无数据竞争、无需仲裁——这是架构约束白送的。

**写折叠。** 同一 calculation 一次运行内对同一字段的多次赋值折叠为一条写记录：`new` 取最终值，`old` 取上一帧提交值。

**多次触发（D3 推论一）。** each 交付下，同一 calculation 在一帧内可能运行多次（多条命中）。各次运行基于同一快照；若多次运行写同一字段，折叠顺序未定义，最终值不确定。因此**帧内聚合一律用 batch 或 fold 表达，禁止用 each 做读-改-写累加**。runtime 可在帧末检测「同字段被同一 calculation 的多次运行写入不同值」并告警（告警等级见 §8 开放问题）。

**无序交付（D3 推论二）。** batch 消费者的逻辑必须与元素顺序无关（把交付视为多重集）。回放确定性仅对顺序无关的消费逻辑成立。

---

## 3. predicate 规范

### 3.1 形状

```
predicate = (scope, condition, delivery)
```

scope 是静态部分：注册期已知、可索引，决定「谁该被叫醒」。condition 是动态部分：逐写 O(1) 判定，决定「值得醒吗」。delivery 是下推的投影与势：决定「递过去什么、递几次」。三段各自绑定一类已知最优的数据结构（§4）。

### 3.2 scope（嗅探范围）

```
scope := own(field)              # 自己实例的某个 cell
       | inst(ref, field)        # 经由自己持有的 ref,盯一个特定实例的 cell
       | type(Entity, field)     # 某 entity 类型的全体实例(通配 id)
       | scope | scope           # 并:任一来源有写入即触发
       | scope & scope           # 合取:同帧都有写入才触发
```

`inst` 的 ref 必须来自自己实例的某个 ref 类型字段（§6.3）。

### 3.3 condition（值条件）

条件可引用的信息是**封闭集**：`new`（本次写入值）、`old`（该 cell 上一帧值，双缓冲免费）、own 行字段（订阅者自己实例的字段，一次点查）、常量（含 `self`，自身实例引用）。结构化 cell 允许字段路径（如 `new.target`）。比较操作数允许常量与 own 字段的标量四则运算（如 `0.3 * own.hp_max`），其代价归入 §4 的「活阈值」一行。

**不允许引用其他实例的行**——那是 join，会破坏成本不变量；需要时物化为 entity（§6.1）。

```
cond := cmp(new|old|own.f, expr)           # = ≠ < ≤ > ≥ ;expr 为常量/own 字段的标量式
      | new in [a,b] | new in {…}
      | changed                            # new ≠ old(与 D2 配合:写即事件,变化要显式问)
      | became(v)                          # old ≠ v ∧ new = v
      | crossed(t, ↑|↓)                    # ↓: old ≥ t ∧ new < t;↑ 对称
      | cond and cond | cond or cond
      | cond and not cond                  # 否定仅作守卫
```

**否定限制的理由。**「没有写入」不是事件，是事件的缺席；独立的 NOT 触发源会把系统拖回每帧轮询。守卫式 `and not` 仍需正触发源，稀疏性不破。「超时 / 静默 N 帧」语义经由时钟 cell 表达（§6.2）。

### 3.4 delivery（投影与势）

```
delivery := each  project(...)        # 每条命中触发一次 calculation
          | batch project(...)        # 整帧聚为一批,一次交付;顺序未定义(D3)
          | fold(sum|count|min|max)   # runtime 增量维护,仅交付聚合值
```

`project` 可投影：`new`、`old`、writer 的实例 id（作为 ref 值交付）、订阅者 own 行字段。交付一律是值快照，不是引用——引用不跨实体，值经由谓词通道流动。

`fold` 是本层最重要的下推形式（增量视图维护）：「全场敌人 HP 总和」写在 calculation 里是每帧 O(N) 扫描，声明为 `fold sum` 后是每写 ±delta。

### 3.5 词汇表的准入标准

一个原语能进入谓词层，**当且仅当**存在一个注册期可建的索引结构，使它的判定摊销在 §4 的成本预算内。词汇表由成本模型生成，不由用例枚举生成。凡绑不上索引的表达力，要么物化为 entity（§6.1），要么留在 calculation。

---

## 4. 成本模型与索引绑定

**下界。** 每条写至少被看一次（路由），每个真触发至少被碰一次（交付），故 Ω(|W| + |F|)；W 为帧写集，F 为实际触发集。

**成本不变量。** 整帧调度成本 = O(|W|·log + |F|)，与谓词总数、实例总数、数据总量无关。这是实现的验收红线。

| 原语 | runtime 结构 | 摊销代价 / 每写 |
|---|---|---|
| own / inst scope | (type, id, field) → 订阅链哈希 | O(1) + 触发数 |
| type scope + 常量阈值条件 | 同 cell 共享排序阈值表 / 区间树 | O(log s + k) |
| type scope + 等值条件 | 值 → 桶 | O(1) + k |
| 条件含 own 字段（活阈值） | 逐订阅者点查 | O(该 cell 订阅者数) |
| changed / became / crossed | 双缓冲旧值 | O(1) |
| `&` 合取 | 每谓词每帧位码 / 计数闩 | O(1) / 子句 |
| batch | 谓词私有帧缓冲 append（无排序，D3） | O(1) |
| fold sum / count | ±delta | O(1) |
| fold min / max | 堆或懒重算 | O(log n) |

s = 该 cell 上的常量条件订阅数，k = 实际命中数。

**诚实退化条款。** 条件参数引用订阅者自身字段时阈值是活的，无法并入共享索引，代价退化为「该 cell 的订阅者数」——仍与全局规模无关；但若某个 cell 被海量个性化条件订阅，这是翻转设计的信号：把中间量物化为 entity（§6.1）。

---

## 5. 注册期编译

注册与注销在帧边界生效。编译流水线：

条件归一化（规范形）→ 等价谓词合并（同 scope + condition 共享一次求值，结果扇出给各 delivery）→ 常量阈值 / 等值聚簇进共享索引 → 单写者冲突检查（D1）→ 否定守卫校验（§3.3）→ 包含关系消解（可选：被蕴含的谓词复用强者的判定结果）。

由「谓词是数据」，以上全部在注册期一次完成；运行期路由不做任何解释执行之外的决策。

---

## 6. 折叠进四层的三件事

### 6.1 join、空间查询、全局排序 → 物化为 entity

这些不进谓词层。规范模式：建一个 singleton entity 作为**索引实体**，用 batch 订阅原始写流，其 calculation 增量维护索引结构，并把查询结果 / 索引切片 write 成自己的字段；下游用普通谓词订阅这些字段。

**索引即实体，视图即数据。** 谓词层因此永远不需要为新场景扩充词汇，而增量维护这条最优路径在用户层永远可达。

示例（空间网格）：`Grid.0` batch 订阅 `type(Unit, position)`，增量维护格子占用，把每格占用写成 `Grid.0` 的字段；近战单位只订阅自己所在格的占用，无 N×M 扫描。batch 无序（D3）在此无害：占用表的维护是顺序无关的。

### 6.2 时钟与每帧逻辑 → runtime 作为 writer

runtime 每帧向内建 cell `Clock.frame` 写帧号。订阅它等价于轮询——代价存在，但显式、可见、自付。这是「每帧都要跑」逻辑的唯一合法出口，保证除此之外的一切计算都由变化驱动。

定时语义的意图实现是 `Clock` 提供的 alarm 机制（内部 timer wheel，O(1) / 帧）：到点写入，订阅者 each 触发。具体接口形态列为开放问题（§8）。

### 6.3 生命周期、ref 与 id 复用

**ref 是 runtime 认识的 cell 类型。** runtime 维护反向表：`target 实例 → { 指向它的 ref cell 集合, 以它为 scope 的 inst 谓词集合 }`。

**创建。** runtime 为新实例写内建 cell `_alive = true`——一次普通 write。观察者用 `type(E, _alive) where became(true)` 感知出生。

**销毁。** 唯一入口是对自身内建 cell `_alive` 写 false（自决）。由写局部性，「杀死他人」必须经由数据流请求（§7 示例 2）；runtime 若提供外部 destroy API，其语义等价于代为写入 `_alive = false`。

**结算（帧边界）。** runtime 沿反向表解除该实例的全部 inst 订阅，并把所有指向它的 ref cell 写成 null——这些同样是普通 write，下一帧持有者用 `became(null)` 收尸。id 归还复用；内部代际号防 ABA（旧帧残留的 ref 值不会误指新住户），对用户不可见。

死亡因此沿普通数据流传播，不需要任何特殊广播机制。这正是「销毁实例应使其引用失效」约束在本架构中的实现：它不是附加机制，是 ref 类型 + 反向表 + 普通 write 的推论。

---

## 7. 示例

```
# 1 血量跌穿 30% —— 边沿触发,不会每帧重复
on    own(hp)
where crossed(0.3 * own.hp_max, ↓)
each  deliver(new, old)
→ flee_calc                          # 写 own(state)

# 2 攻击 —— 跨实体交互的唯一通道:写自己,被对方嗅探
#   攻击方 calculation 写 own(attack_out) = {target: ref, dmg: 5}
on    type(Attacker, attack_out)
where new.target = self
each  deliver(new.dmg)
→ take_damage_calc                   # 受击方写 own(hp);hp 归零时写 own(_alive) = false

# 3 空间网格 —— 索引即实体,挂在 Grid.0
on    type(Unit, position)
batch deliver(writer_id, old, new)   # 无序交付(D3),维护逻辑必须顺序无关
→ grid_update_calc                   # 写 own 的格子占用字段

# 4 Boss 血条 —— 增量聚合
on    type(Enemy, hp)
fold  sum
→ boss_bar_calc                      # 写 own(total_hp),供 UI 实体订阅

# 5 目标死亡感知 —— 收尸
on    own(target)                    # target 为 ref 类型,destroy 结算时被 runtime 写成 null
where became(null)
each
→ retarget_calc
```

---

## 8. 不变量清单与开放问题

**不变量（实现的验收标准）。** 四层封闭：一切机制是四层的推论。触发源唯一：只有上一帧的 write。写局部 + 单写者（D1）。写即事件（D2）。快照读：本帧写入本帧不可见。谓词是数据：注册期定型、可编译。成本不变量：O(|W|·log + |F|)，与谓词总数、实例总数无关。交付无序（D3）：消费逻辑必须顺序无关。

**开放问题。** 其一，`Clock` 的 alarm 接口形态与 timer wheel 的对接细节。其二，`project` 是否允许投影侧运算（条件侧标量四则已允许，§3.3）。其三，`fold` 是否向用户开放自定义幺半群；若开放，需约定可逆性或重算策略（min/max 已是非可逆先例）。其四，「同字段被同一 calculation 多次运行写入不同值」（§2）的检测开销与告警等级：静默折叠、警告、还是按错误处理。
