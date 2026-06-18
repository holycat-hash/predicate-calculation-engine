# 设计备忘：执行层 kernel-IR seam

> 状态：**已落地迁移第 1-4 步**。core 现在已有 `KernelIr` / `KernelOp`、
> `Tier::Kernel` 的注册期校验、SoA `KernelColumn` 批执行、`KernelBackend` trait，
> 以及默认 `ScalarKernelBackend`。SIMD/GPU 实现仍留给可选外部或 feature-gated 后端。镜像英文见
> [../docs/design-kernel-ir-seam.md](../docs/design-kernel-ir-seam.md)。

## 1. 问题

谓词层已经完成「行为 → 数据」这一步：`Cond` / `Expr` 在注册期降为扁平后缀程序
（`route.rs` 的 `CompiledCond` / `CompiledExpr`），运行期是对连续 slice 的紧循环、可被
向量化、可被审查。执行层现在也对 kernel 子集补上了同一条 seam：`KernelIr` 是可由 SoA
backend 执行的后缀数据程序；图灵完备 calculation 本体仍保留不透明 `Box<dyn Fn>` 兜底。
核心 opaque/backend 缺口已经收敛，剩余的是可选硬件 backend 的实现工作：

- **D4 副作用封闭对 IR 可机检、对闭包仍是契约**：闭包内仍可能偷偷做 ambient I/O / log /
  static / RNG；IR 本体则按构造表达不了这些效应。
- **C1 kernel 档已有可执行列批**：注册期要求 IR，运行期把每个 calc group 作为 lane
  批跑在 SoA 输入 / 输出列上。
- **C3 GPU 驻留已成为 backend 选择输入**：`Schedule` / `Profile` 解析出驻留分区与 GPU
  建议，runtime 把该 hint 传给已注册的 `KernelBackend`。没有 GPU backend 时，默认 scalar
  backend 仍是精确兜底。
- **SIMD / GPU 派发已有入口**：backend 作者拿到的是 `(IR, lanes, input columns,
  residency hint)`，而不是不透明闭包。

按架构哲学（白送做满 / 有代价给接口 / 找让最多优化穿过去的**最少 seam 集合**），
SIMD、GPU、residency、migration 这些「有代价」优化全都要求同一个前提：**能把 calc 行为
当数据看**。kernel-IR seam 就是那个最少 seam；第 1-4 步现在已经补完 core 的数据并行
后端故事。

## 2. 切口：行为 → 数据（kernel IR）+ 可插拔派发

两部分：

1. **kernel IR**：一个小的、注册期可建的数据形式，表达 **kernel 子集**的 calc 本体
   （C1：无 spawn、无动态分配、无分支发散、副作用只经 ctx 写自己实例）。它是
   `CompiledExpr` 在执行层的对偶——同样是「读输入 → 算 → 写输出」的后缀 / 三地址程序，
   但允许**多个输出写**（calc 可写多个声明字段）。
2. **可插拔派发**：一个 `KernelBackend` 接口，吃（kernel IR、lanes、输入列、输出列、
   residency hint）并执行。core 提供 SoA scalar backend；后续可选实现是 SIMD、GPU。

**Turing 完备的 calc 本体仍走 `Box<dyn Fn>` 兜底**——IR 只覆盖 kernel 子集，不强迫一切入
IR。这与谓词层一致：谓词是封闭代数（数据），calculation 是图灵完备（代码）；kernel IR
只是把 calculation 中**恰好是纯数据流的那一类**也降为数据，其余保持闭包。

## 3. IR 的范围

kernel IR 描述一个**逐实例 kernel**：

```
inputs  : 一组列读（own 字段 / 投影输入 / 时钟标量）
body    : 算术 / 比较 / 选择（无循环、无分支发散——用 select 而非 jump）
outputs : 一组列写（仅自己实例的声明字段，D1）
```

这恰好是 SIMD lane / GPU thread 的形状：N 个实例 = N 条 lane，输入列 = SoA 输入缓冲，
输出列 = SoA 输出缓冲。底层 SoA 列内核已存在（`src/column.rs` 的类型化列 + `genslots.rs`
存活槽），是 kernel IR 的物化底座。

不进 IR 的（留闭包）：spawn / destroy、ref 点查跨实体、任意控制流、调用外部库。

## 4. 迁移路径（增量、不破坏现状）

每步都是**附加**的，闭包兜底始终保留；每步落地后全套测试应逐字绿（解释器语义 == 闭包）。

- **第 0 步（已完成）**：谓词层「行为 → 数据」已落（`CompiledCond` / `CompiledExpr`）。
  先例与求值内核（后缀程序 + 值栈）已在，可复用其形。
- **第 1 步（已完成）：IR + 标量解释器**。定义 kernel IR enum，写一个标量解释器。calc 注册期可
  **可选**附带 IR 本体（与 `CalcFn` 并存）；提供 IR 的走解释器，否则走闭包。验收：对同一
  calc，IR 路径与闭包路径产出逐字相同。
- **第 2 步（已完成）：C1 接入 + D4 机检**。C1 标注的 calc 要求提供 IR；注册期校验 IR 满足 kernel
  约束。**IR 本体按构造无 ambient 副作用 ⇒ D4 对 IR 子集从契约升级为机检保证**。
- **第 3 步（已完成）：SoA 列执行**。把 kernel 跑在输入列上、产出输出列（而非逐实例建 ctx）。复用
  `column.rs` 的类型化列。这是 SIMD 的前提，也兑现 C1 的「kernel 批」。
- **第 4 步（已完成）：可插拔后端**。`KernelBackend` trait + 默认
  `ScalarKernelBackend`；可注册可选 SIMD 后端（`std::simd` 或 crate）、GPU 后端（`wgpu`，
  feature 门控）。`Schedule` / `Profile` 的 C3 驻留建议成为后端选择的真实输入。

## 5. 收益（落地后各档从「seam + 建议」升级为「真实接入」）

| seam 前 | 当前 / 剩余 |
|---|---|
| D4 = 不可机检契约 | IR 子集**按构造**无 ctx 外副作用；闭包仍靠契约 |
| C1 = 仅标注 | IR 执行已是 SoA kernel 批 |
| C3 = 仅建议（无后端） | residency 已进入 backend 选择；scalar fallback 精确兜底 |
| SIMD / GPU = 无入口 | `KernelBackend` 已是入口；硬件 backend 是可选实现 |
| 并行合法性 = 假设 effect-confinement | IR 子集可证；闭包子集仍假设（缩小了「相信」面） |

## 6. 边界（非目标）

- **不把一切塞进 IR**：图灵完备本体保留闭包兜底。IR 是「恰好纯数据流」那类的快路，不是
  通用脚本语言。
- **不在 core 造硬件后端**：SIMD/GPU 后端是「有代价」优化，交接口（`KernelBackend`）
  给开发者按硬件 / 负载选。core 提供精确 SoA scalar fallback 与 backend trait。这与
  render 提交 seam、空间索引 helper 同准则。
- **不改谓词层**：谓词早已是数据；本备忘只补执行层的对偶。

## 7. 与既有件的关系

- `route.rs` `CompiledCond` / `CompiledExpr`：谓词层的「行为 → 数据」先例与可复用求值形。
- `src/column.rs` / `src/genslots.rs`：SoA 列 + 存活槽，kernel IR 的物化底座。
- `runtime/mod.rs` `Schedule` / `ScheduleGroup` / `Profile`：已按 calc 分组并解析 C1/C3
  分区，落地后成为派发驱动。
- `Snapshot` / `restore`（GGPO）：IR 本体确定 ⇒ 助力 Canonical 确定重放。
- render `run_continuous`（B3 两阶段快照 + 并行）：已是「读冻结列 → 写缓冲列」的形状，
  是 kernel-IR 列执行在 render 侧的预演。

## 8. 工作量与建议

core seam 已实现并测试：IR 等价、C1/D4 校验、SoA 列批、backend 选择都已落地。下一轮最
有价值的推进是可选 backend：SIMD lane 特化、feature-gated GPU 执行，以及 fallback /
hysteresis 策略。
