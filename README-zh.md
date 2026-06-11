# PredicateCalculationEngine（PCE）

一个纯数据驱动的谓词计算引擎（Rust 实现），面向游戏逻辑等高频帧驱动场景。
整个系统有且只有四层抽象——**runtime / entity / calculation / predicate**——任何新需求必须折叠进这四层，禁止引入第五种概念：没有消息、没有事件总线、没有回调、没有全局函数。

唯一的触发源是「上一帧的写入」。轮询、消息、事件全部被统一为「对某个 cell（实体实例的一个字段）的 write」。

## 核心设计

| 层 | 职责 |
|---|---|
| **runtime** | 唯一的调度者与索引持有者：双缓冲、写集路由、谓词索引、fold 增量状态、实例生命周期 |
| **entity** | 实例化的最小单位（`entityname.id`）；全局状态以 singleton entity 表达（如 `Clock.0`）|
| **calculation** | 图灵完备的业务代码；输入是前置 predicate 的交付（值快照），输出只能写**自己实例**的字段 |
| **predicate** | 注册期定型的声明式三段结构 `(scope, condition, delivery)`，封闭代数、可编译、可索引 |

三条钉死的决议：

- **D1 单写者制** —— 每个字段静态归属唯一一个 calculation，注册期检查，冲突即错。
- **D2 写即事件** —— write 一律产生事件，无论值是否变化；「值真的变了」由条件 `changed` 显式表达。
- **D3 batch 不排序** —— batch 交付顺序未定义，消费逻辑必须顺序无关（多重集语义）。

由此白送的性质：快照读（本帧写入本帧不可见）、执行阶段无数据竞争可并行、反馈环天然展开为帧间 ping-pong，以及**成本不变量**——整帧调度成本 O(|W|·log + |F|)，与谓词总数、实例总数、数据总量无关。

## 示例

文档 §7 的谓词 DSL 风格：

```
# 血量跌穿 30% —— 边沿触发，不会每帧重复
on    own(hp)
where crossed(0.3 * own.hp_max, ↓)
each  deliver(new, old)
→ flee_calc                          # 写 own(state)

# 攻击 —— 跨实体交互的唯一通道：写自己，被对方嗅探
on    type(Attacker, attack_out)
where new.target = self
each  deliver(new.dmg)
→ take_damage_calc                   # 受击方写 own(hp)
```

对应的 Rust API 用法见 [src/main.rs](src/main.rs)（`pce-demo` 可执行示例）。

## 快速开始

```powershell
cargo run            # 运行 demo（§7 示例 1 + 2：攻击数据流 + 血量边沿触发）
cargo test           # 运行 tests/ 下的场景用例
```

## 仓库结构

```
src/
  lib.rs             # crate 入口与核心导出
  entity.rs          # 实体类型 / 实例 / 字段 / cell 地址
  predicate.rs       # 谓词代数：scope / condition / delivery
  calculation.rs     # calculation 注册与执行上下文
  value.rs           # cell 值类型
  runtime/           # 调度器：双缓冲、写集路由、时钟
docs/                # 英文文档
docs-zh/
  PCE文档.md          # 架构总纲（四层抽象、帧模型、成本模型、不变量清单）
  README.md          # 「刁钻逻辑切分方法集」索引（23 篇场景文档）
  01..23-*.md        # 各场景：超时、同帧争用、连锁反应、原子消耗、双人交易……
tests/               # 由场景文档转化的可运行用例
```

## 文档

- 架构总纲：[docs-zh/PCE文档.md](docs-zh/PCE文档.md) —— 四层抽象、帧模型与数据流、predicate 规范、成本模型与索引绑定、注册期编译、不变量清单与开放问题。
- 场景方法集：[docs-zh/README.md](docs-zh/README.md) —— 23 篇「只靠 predicate + calculation + entity 切分」落地刁钻逻辑的文档（仇恨嘲讽、连击取消、投射物、时间膨胀、对称交易等），每篇含正确性论证与成本分析，并附通用手法速查。

## 测试覆盖

`docs-zh/01..23-*.md` 的 23 篇场景文档均已转成 `tests/` 下的 Rust 集成测试。每个测试文件对应一篇场景文档，验证文档里的关键切分、不变量与 D1/D2/D3 约束。

```powershell
cargo test
```

## 状态

早期原型（0.1.0，无第三方依赖）。runtime 仍是脚手架阶段，部分索引优化与谓词编译收敛还在 `TODO` 中；但 23 篇场景文档已经全部有可运行集成测试，可作为当前行为与设计约束的回归网。
