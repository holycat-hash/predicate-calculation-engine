# PredicateCalculationEngine（PCE）

**语言：** [English](README.md) | 中文

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

## 作为依赖使用

本 crate 是纯库（`publish = false`，名称 `pce` 为占位），路径依赖引入：

```toml
[dependencies]
pce = { path = "../predicate-calculation-engine" }
# 可选：执行阶段并行（D1 + 写局部性保证无竞争，架构白送的合法性）
# pce = { path = "...", features = ["parallel"] }
```

库入口只暴露四层核心：`runtime` / `entity` / `calculation` / `predicate`。
早期“近似文档 DSL”的 Rust API 形态已移到
[docs-zh/original-api-shape.rs](docs-zh/original-api-shape.rs)，只作为教学说明书，
不再作为模块导出，也不参与库编译。

## 示例

文档 §7 的谓词 DSL：

```
# 血量跌穿 30% —— 边沿触发，不会每帧重复
on    own(hp)
where crossed(0.3 * own.hp_max, ↓)
each  deliver(new, old)
→ flee_calc                          # 写 own(state)
```

对应的纯库核心 API 写法：

```rust
use pce::predicate::{own, own_field};
use pce::{
    Cond, Delivery, Dir, Expr, FieldDef, Predicate, Proj, Runtime, ValRef, Value,
};

let mut rt = Runtime::new();
let unit = rt.register_entity_type(
    "Unit",
    vec![
        FieldDef::new("hp", Value::Int(100)),
        FieldDef::new("hp_max", Value::Int(100)),
        FieldDef::new("state", Value::str("idle")),
    ],
    false,
);

let (f_hp, f_hp_max, f_state) = (
    rt.field(unit, "hp"),
    rt.field(unit, "hp_max"),
    rt.field(unit, "state"),
);
rt.register_calculation(
    "flee",
    unit,
    Predicate::new(
        own(f_hp),
        Cond::Crossed(
            Expr::Mul(
                Box::new(own_field(f_hp_max)),
                Box::new(Expr::Val(ValRef::Const(Value::Float(0.3)))),
            ),
            Dir::Down,
        ),
        Delivery::Each(vec![Proj::New(vec![]), Proj::Old(vec![])]),
    ),
    &[f_state],
    Box::new(move |ctx, _| ctx.write(f_state, "fleeing")),
)
.unwrap();
```

跨实体攻击（`new.target = self` 注册期识别为等值快路，按 ref 点查）、
fold 增量聚合、每帧 ECS 等完整用法见 [examples/demo.rs](examples/demo.rs)。

## 优化

**白送优化（A 层，无条件生效）**：SoA 列存（`(type, field)` → 稠密列）；
双缓冲 = 单存储 + 写日志，帧界提交；type scope 的值桶（常量等值，O(1)+k）、
共享排序阈值表与 crossed 区间查询（O(log s + k)）；等价条件合并求值；
fold 增量维护（sum/count ±delta，min/max 多重集）；`type(Clock, frame)` +
恒真条件注册期识别为经典 ECS system，跳过路由走稠密列遍历；路由 scratch
跨帧复用；免费 profiler（`Runtime::profile`——路由输入就是每 cell 写频，
D2 买单，B 层自适应的遥测前提）。

**开发者档位（C 层，真正有代价的选择）**：

| 档位 | 入口 | 代价 |
|---|---|---|
| C1 执行档位 | `.tier(Tier::Kernel)` | 受限子集（禁 spawn 等动态分配），发散风险自负 |
| C2 读集声明 | `.reads(["hp_max"])` | 声明负担；换热冷分离与预取精度 |
| C3 驻留划分 | `.residency(Residency::Gpu)` | pin 无静态正解，配合 profile + 滞回 |
| C4 确定性 | `rt.set_determinism(Canonical)` | batch 规范序排序成本；lockstep/回放买单 |
| C5 检测档位 | `rt.set_detect(Strict/Warn/Silent)` | Strict/Warn 污染热路径；默认跟随构建档 |
| C6 行身份 | `.compact()`（默认稳定行） | 稳定行留洞 vs 压缩行死亡重映射 |

## 快速开始

```powershell
cargo run --example demo   # §7 示例 1+2+4：攻击数据流、边沿触发、增量聚合
cargo test                 # 场景用例 + 核心 API / 优化行为验证
cargo test --features parallel   # 执行阶段并行（rayon）下的同一套测试
```

## 仓库结构

```
src/
  lib.rs             # crate 入口、四层核心导出、优化档位总览
  entity.rs          # 实体类型 / 实例 / 字段 / cell 地址
  predicate.rs       # 谓词代数：scope / condition / delivery
  calculation.rs     # calculation 执行上下文（C1/C2 检测感知）
  value.rs           # cell 值类型
  runtime/
    mod.rs           # 调度器：注册期编译、帧循环、档位、profiler
    store.rs         # SoA 列存 + 行身份策略（C6）
    route.rs         # 路由索引：值桶 / 阈值表 / 等价合并 / fold
    clock.rs         # 时钟与 alarm（timer wheel）
examples/demo.rs     # 纯库核心 API 完整示例
docs-zh/
  PCE文档.md          # 架构总纲（四层抽象、帧模型、成本模型、不变量清单）
  original-api-shape.rs # 原始 API 形态说明（教学参考，不参与库编译）
  README.md          # 「刁钻逻辑切分方法集」索引（23 篇场景文档）
  01..23-*.md        # 各场景：超时、同帧争用、连锁反应、原子消耗、双人交易……
tests/               # 场景用例 + 核心 API / 优化行为验证
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

0.1.0，纯库 crate（默认零第三方依赖；`parallel` feature 引入 rayon）。
§4 成本表的索引绑定已逐项落地（own/inst 哈希链、值桶、共享阈值表、fold
增量、合取闩），A 层白送优化全部生效，C 层档位入口全部暴露；23 篇场景
文档的集成测试 + 核心 API/优化行为测试构成回归网。SIMD kernel 代码生成与 GPU
驻留 backend 属 C1/C3 的后端实现，结构（列存、阈值表、Tier/Residency
标注、profiler 边权遥测）已就位，留待接入。
