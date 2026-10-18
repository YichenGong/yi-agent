# 设计：协议 DTO 由 Rust 生成 TypeScript（ts-rs codegen）

日期：2026-10-17
状态：待评审
关系：本文来自 2026-10-17 的仓库冗余审计（P1-1）。审计结论：桌面端的
`desktop/src/lib/protocol.ts` 手工镜像 Rust 的
`yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`，且全仓没有任何代码生成，
存在持续漂移风险。

## 1. 背景与问题

`protocol.ts` 的文件头自述：

> These mirror the Rust definitions in
> `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`.

现状：

| 事实 | 值 |
|---|---|
| `protocol.rs` | 1092 行，17 个 `pub struct`/`pub enum` |
| `protocol.ts` | 413 行，34 个类型声明（手写） |
| 生成机制 | **无**（全仓 `grep ts-rs\|typeshare\|specta\|schemars` 无命中） |
| CI 校验 | **无** |
| 唯一的"同步"测试 | `protocol.test.ts`（20 行）：构造两个字面量对象看能否编译，**无法发现漂移** |

这是典型的"双真相源"：Rust 改了字段名，TS 不会报错，直到运行时 RPC 静默失配。

## 2. 目标与非目标

### 目标

1. 用 `ts-rs` 从 `protocol.rs` **自动生成** TS 类型到
   `desktop/src/lib/generated/`。
2. 桌面端 `protocol.ts` 改为**再导出**生成物，删除手写重复定义。
3. 校验生成物与提交物一致（防止"改了 Rust 忘了重新生成"）。

### 非目标

- 不改 RPC 协议本身、不改字段名/大小写约定。
- 不为 `protocol.rs` 之外的类型生成（如 `core`/`store` 的内部 DTO）。YAGNI。
- 不引入运行时校验（zod 等）。

## 3. 设计

### 3.1 工具与落点（已实测可行性）

`ts-rs` 11.1.0 与 `ts-rs-macros` 11.1.0 **已在本地 cargo 缓存**（可 `--offline` 构建）。
我给 `Scope`/`ImageRef`/`Item` 等加 derive 做过 spike：`cargo test -p yi-agent-app-server
export_bindings` 能生成正确的 `.ts`，例如

```ts
export type Scope = "observe" | "control" | "admin";
```

即 `#[serde(rename_all = "lowercase")]` 被正确翻译。

- 依赖：`yi-agent-app-server` 的 `[dependencies]` 加
  `ts-rs = { version = "11", features = ["serde-compat"] }`。
- 每个要导出的类型加 `TS` derive 与
  `#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]`
  （**相对路径以 `CARGO_MANIFEST_DIR` 即 `yi-agent-rs/crates/yi-agent-app-server/`
  为基准**；实测 `../../../` 会落到 `yi-agent-rs/desktop/...`，必须四层）。

### 3.2 三个必须显式处理的 serde/TS 差异（spike 实测得出）

ts-rs 默认翻译与线上 JSON 有三处不符，必须逐处标注：

1. **`serde_json::Value` 字段**：`TS` 不为 `Value` 实现，编译报
   `the trait bound serde_json::Value: TS is not satisfied`。这些字段（`params`、
   `data`、`payload` 等）要加 `#[ts(type = "unknown")]`。
2. **`u64` 字段**：生成 `bigint`，而手写 TS 是 `number`（且前端确实按 `number` 用）。
   对线上是小整数的（`size`、`input_tokens` 等）加 `#[ts(type = "number")]` 保持语义。
3. **`Option<T>` + `skip_serializing_if = "Option::is_none"`**：ts-rs 默认生成
   `T | null`，但线上是**字段缺失**。加 `#[ts(optional)]` 生成 `field?: T`，与线一致。

### 3.3 递归依赖（多米诺）

`Item` 引用 `Vec<Attachment>`、`Notification` 引用 `Item` 等，**任一嵌套类型缺 `TS`
都会编译失败**。因此必须给全部相关类型加 derive，不能只给顶层。spike 中逐个加会
反复报错，故计划采用"一次性扫全 `protocol.rs` 的 derive 行"的做法。

`RequestEnvelope` 等含 `serde_json::Value` 的类型同样需要 §3.2 第 1 条。

### 3.4 前端接缝

- `desktop/src/lib/generated/` 下的文件**提交进版本控制**（这样前端构建不依赖 Rust
  工具链；且 CI 能校验"生成物是否最新"）。
- `desktop/src/lib/protocol.ts` 保留为**再导出层**：

  ```ts
  export type { Item } from "./generated/Item";
  export type { Attachment } from "./generated/Attachment";
  // …等
  ```

  这样现有 `import { Item } from "./lib/protocol"` 的调用点**零改动**。

### 3.5 一致性校验（防漂移）

在 `justfile` 加一个配方，并把生成物纳入 CI：

```make
# 校验 ts-rs 生成物与提交物一致（改了 protocol.rs 必须重新生成）
bindings-check:
    cargo test --all-features -p yi-agent-app-server export_bindings
    git diff --exit-code -- ../desktop/src/lib/generated
```

- 本地：`just bindings-check`。
- CI：并入 `just ci`（`yi-agent-rs/justfile` 的
  `ci: fmt-check lint test build bindings-check`）。这样 GitHub PR / GitLab
  push / GitLab MR 三条路径都跑到该门禁，无需在 `.github/workflows/ci.yml`
  里单列一步。（早期实现确实另加了一步 `just bindings-check`，后因会重复执行
  而合入 `just ci`。）

> 注意 `export_bindings` 测试由 ts-rs 自动生成（每个带 `export` 的类型一个
> `export_bindings_<name>` 测试），名字随类型名变化，所以配方用**测试名前缀过滤**
> 而非枚举名字。

## 4. 测试策略

1. **生成物是编译通过的真 TS**：`cd desktop && node_modules/.bin/tsc --noEmit`
   （tsc 6.0.3 在 `desktop/node_modules/.bin`，已实测可用）。
2. **前端行为不变**：`cd desktop && node_modules/.bin/vitest run`
   （基线 89 文件/1113 测试全绿，8.57s）。
3. **一致性**：`just bindings-check` 在生成物已提交时应无 diff。
4. **漂移能被抓到**（负向验证）：临时改 `protocol.rs` 一个字段名 → `bindings-check`
   必须报 diff → 改回。

## 5. 验收标准

- `desktop/src/lib/generated/` 下每个 `protocol.rs` 的 `pub` 类型各有一个 `.ts`。
- `protocol.ts` 不再有手写字段定义（只剩再导出）。
- `tsc --noEmit` 与 `vitest run` 全绿。
- `just bindings-check` 干净；人为漂移时能失败。
- `cargo clippy --workspace --all-targets -- -D warnings` 无警告。
