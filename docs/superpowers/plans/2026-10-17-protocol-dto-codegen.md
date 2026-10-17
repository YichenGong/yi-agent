# 协议 DTO codegen Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用 `ts-rs` 从 `protocol.rs` 生成 TS 类型到 `desktop/src/lib/generated/`，把 `protocol.ts` 收缩为再导出层，并加一致性校验。

**Architecture:** `yi-agent-app-server` 加 `ts-rs` 依赖；`protocol.rs` 的每个 `pub` 类型加 `TS` derive 与 `#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]`；生成物提交进版本控制；`protocol.ts` 只做 `export type { … } from "./generated/…"`；`justfile` 加 `bindings-check` 并把生成物纳入 CI 校验。

**Tech Stack:** Rust（`ts-rs` 11 + `serde-compat`，**已在本地 cargo 缓存，可 `--offline`**）；前端 `tsc` 6.0.3 与 `vitest` 3.2.7（在 `desktop/node_modules/.bin`）。

**Spec:** `docs/superpowers/specs/2026-10-17-protocol-dto-codegen-design.md`

## Global Constraints

- 行号以基线 `8e220ef2` 为准；改动前 `grep -n` 复核锚点。
- 环境（Rust）：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`，cargo 加 `--offline`。
- 环境（前端）：`export PATH="/opt/homebrew/bin:$PATH"`（node v23.11.0 / npm 10.9.2 在此），
  命令用 `desktop/node_modules/.bin/tsc`、`desktop/node_modules/.bin/vitest`。
- **`export_to` 必须是四层 `../../../../`**（实测三层会落到 `yi-agent-rs/desktop/`）。
- **不改任何字段名、不改 serde 的大小写约定**：codegen 必须复现线上 JSON 形状。
- **生成物提交进 git**（`desktop/src/lib/generated/` 不写进 `.gitignore`）。
- 只显式 `git add <path>`。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` — 加 `ts-rs` 依赖。
- `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` — 全部 `pub` 类型加 `TS` derive + `#[ts(...)]`；`Value`/`u64`/`Option` 字段逐处标注。
- `desktop/src/lib/generated/*.ts` — **新生成**（提交）。
- `desktop/src/lib/protocol.ts` — 改为再导出。
- `yi-agent-rs/justfile` — 加 `bindings-check`。
- `.github/workflows/ci.yml` — `just ci` 后加 `just bindings-check`。

---

### Task 1: 加 ts-rs 依赖并让全部协议类型可导出

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`

**Interfaces:**
- Produces: `desktop/src/lib/generated/<TypeName>.ts`，每个 `protocol.rs` 的 `pub` 类型一份。

- [ ] **Step 1: 加依赖**

在 `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` 的 `[dependencies]` 段顶部加：

```toml
ts-rs = { version = "11", features = ["serde-compat"] }
```

- [ ] **Step 2: 记录改动前的类型清单**

Run: `cd yi-agent-rs && grep -nE "^pub (struct|enum) " crates/yi-agent-app-server/src/protocol.rs`

Expected: 17 行，含 `Scope`、`RequestId`、`RequestEnvelope`、`ResponseEnvelope`、
`ClientResponse`、`ReverseRequest`、`RpcError`、`Notification`、`AgentChild`、
`AgentTraceRow`、`NotificationEnvelope`、`TurnStatus`、`ThreadStatus`、`Attachment`、
`ImageRef`、`Item`、`ToolStatus`。记下这份清单。

- [ ] **Step 3: 在 import 区加 `TS`**

在 `protocol.rs` 顶部：

```rust
use serde::{Deserialize, Serialize};
use ts_rs::TS;
```

- [ ] **Step 4: 给全部 `pub` 类型加 `TS` derive**

对清单里**每一个** `pub struct`/`pub enum`：在其 `#[derive(...)]` 里加 `TS`，并在
derive 行之后、`#[serde(...)]` 行之前加：

```rust
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
```

例如 `Scope`：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub enum Scope {
    Observe,
    Control,
    Admin,
}
```

> 带 `<'a>` 生命周期的类型（`ReverseRequest<'a>`、`NotificationEnvelope<'a>`）若无
> 可导出字段可**跳过** `TS`（它们含借用，ts-rs 支持有限）。先给其余类型加；若编译报
> 某类型缺 `TS`，按 §"递归依赖"给该被引用类型补上。

- [ ] **Step 5: 编译，按报错补标注（`Value` 字段）**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo build -p yi-agent-app-server --offline 2>&1 | grep -E "^error" -A3 | head -40`

Expected: 会报若干 `the trait bound serde_json::Value: TS is not satisfied`。
对每个报错的字段加标注，例如：

```rust
    #[ts(type = "unknown")]
    pub params: Value,
```

- [ ] **Step 6: 编译直到无错**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo build -p yi-agent-app-server --offline 2>&1 | grep -E "^(error|warning)" | head -20`

Expected: 无输出。

- [ ] **Step 7: 生成并检查产物**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-app-server --offline export_bindings 2>&1 | tail -5
ls ../desktop/src/lib/generated/
```

Expected: 生成 `.ts` 文件；且**没有** `yi-agent-rs/desktop/` 目录被创建
（若出现，说明 `export_to` 层数不对，改回四层 `../../../../`）。

- [ ] **Step 8: fmt + commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent-app-server && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/Cargo.toml yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs yi-agent-rs/Cargo.lock desktop/src/lib/generated/
git commit -m "feat(app-server): export protocol DTOs to TypeScript with ts-rs"
```

---

### Task 2: 校正 u64 与 Option 的线上形状

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
- Modify: `desktop/src/lib/generated/*.ts`（重新生成）

**Interfaces:**
- Consumes: Task 1 的生成管线。

- [ ] **Step 1: 找出 u64 字段**

Run: `cd yi-agent-rs && grep -nE "pub [a-z_]+: u64" crates/yi-agent-app-server/src/protocol.rs`

Expected: 至少 `Attachment.size`、`ImageRef.size`，以及 token 计数类字段。

- [ ] **Step 2: 找出手写 TS 里对应的类型，确认线上是 number**

Run: `cd desktop && grep -nE "size|tokens" src/lib/protocol.ts | head -20`

Expected: 手写版是 `number`。据此，给这些 `u64` 字段加：

```rust
    #[ts(type = "number")]
    pub size: u64,
```

> 判断依据：线上值来自 JSON 数字且实际范围远小于 2^53；前端本就按 `number` 使用。
> 若某字段确实可能超 2^53（本协议内没有），保持 `bigint` 并同步改前端。

- [ ] **Step 3: 处理 `skip_serializing_if` 的 Option**

Run: `cd yi-agent-rs && grep -nB2 "skip_serializing_if" crates/yi-agent-app-server/src/protocol.rs`

对每个 `#[serde(skip_serializing_if = "Option::is_none")]` 的字段，加 `#[ts(optional)]`：

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mime: Option<String>,
```

Expected: 生成的 TS 里该字段为 `mime?: string`（不再是 `mime: string | null`），
与线上"缺字段"一致。

- [ ] **Step 4: 重新生成并目检三个代表类型**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-app-server --offline export_bindings 2>&1 | tail -3
cat ../desktop/src/lib/generated/Attachment.ts
cat ../desktop/src/lib/generated/ImageRef.ts
cat ../desktop/src/lib/generated/Item.ts | head -30
```

Expected: `Attachment.size`/`ImageRef.size` 为 `number`；`mime`/`detail` 为 `?:`；
`Item` 的联合以 `type: "userMessage" | …` 判别（camelCase 标签保留）。

- [ ] **Step 5: commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs desktop/src/lib/generated/
git commit -m "fix(app-server): make exported TS match the wire shape for u64 and optional fields"
```

---

### Task 3: 前端改用生成物（protocol.ts 变再导出层）

**Files:**
- Modify: `desktop/src/lib/protocol.ts`

**Interfaces:**
- Consumes: `desktop/src/lib/generated/*.ts`（Task 1、2）。

- [ ] **Step 1: 记录改动前的调用点与基线**

```bash
cd desktop && grep -rln "from \"\(\.\|\.\.\)/\(lib/\)\?protocol\"" src | head -20
export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -5
```

Expected: `tsc` 无输出；`vitest` 89 文件/1113 测试全绿。记下调用点清单。

- [ ] **Step 2: 把 protocol.ts 重写为再导出**

保留文件头的说明注释（更新为"由 ts-rs 生成、此处只再导出"），把全部手写
`export interface/type …` 替换为对生成物的再导出。**必须覆盖原文件导出的每一个名字**
（以改动前的 `grep "export " src/lib/protocol.ts` 为准），例如：

```ts
/**
 * Wire-protocol types for the `yi-agent app-server` JSON-RPC 2.0 interface.
 *
 * These are **generated** from `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
 * by ts-rs into `./generated/`. This module only re-exports them so existing
 * imports (`from "./lib/protocol"`) keep working. Do not hand-edit the shapes —
 * edit the Rust and run `just bindings-check`.
 */

export type { Scope } from "./generated/Scope";
export type { RequestId } from "./generated/RequestId";
export type { Attachment } from "./generated/Attachment";
export type { ImageRef } from "./generated/ImageRef";
export type { Item } from "./generated/Item";
// …补齐改动前列出的**每一个**导出名
```

> 原文件里若有**本地补充的辅助类型**（不是 protocol.rs 的镜像，例如前端专用的
> `RequestFn` 之类），保留在本文件里原样不动——只替换掉属于 Rust 镜像的那部分。

- [ ] **Step 3: 类型检查通过**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/tsc --noEmit 2>&1 | tail -20`

Expected: 无输出。若报某类型在某处"不存在"，说明 Step 2 漏了一个导出名，补上。

- [ ] **Step 4: 前端测试全绿**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -5`

Expected: 与 Step 1 相同：89 文件/1113 测试全绿。

- [ ] **Step 5: commit**

```bash
git add desktop/src/lib/protocol.ts
git commit -m "refactor(desktop): re-export generated protocol types instead of hand-mirroring"
```

---

### Task 4: 一致性校验（justfile + CI）

**Files:**
- Modify: `yi-agent-rs/justfile`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: `just bindings-check`。

- [ ] **Step 1: 加 justfile 配方**

在 `yi-agent-rs/justfile` 的 `ci` 配方之后加：

```make
# 校验 ts-rs 生成物与提交物一致（改了 protocol.rs 必须重新生成）。
# `export_bindings` 是 ts-rs 自动生成的测试名前缀，按前缀过滤即可覆盖全部类型。
bindings-check:
    cargo test --all-features -p yi-agent-app-server export_bindings
    git diff --exit-code -- ../desktop/src/lib/generated
```

- [ ] **Step 2: 本地正向验证**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && just bindings-check 2>&1 | tail -6`

Expected: 无 diff，退出码 0。

- [ ] **Step 3: 本地负向验证（确认真能抓到漂移）**

```bash
cd yi-agent-rs
# 人为漂移：给一个字段改名（改完立刻改回）
sed -i.bak 's/pub size: u64,/pub size: u64, \/\/ drift/' crates/yi-agent-app-server/src/protocol.rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt -p yi-agent-app-server
just bindings-check; echo "exit=$?"
# 复原
mv crates/yi-agent-app-server/src/protocol.rs.bak crates/yi-agent-app-server/src/protocol.rs
```

Expected: 该次 `bindings-check` 退出码非 0（`git diff --exit-code` 发现生成物变化）。
> 说明：本步骤也可用"改一个 `#[serde(rename)]`"之类更直接的字段级改动；关键是
> **必须观察到一次非 0 退出**，否则校验形同虚设。

- [ ] **Step 4: 加进 CI**

在 `.github/workflows/ci.yml` 的 `ci` job 里，`Run CI` 一步之后加：

```yaml
      - name: Check generated TypeScript bindings
        working-directory: yi-agent-rs
        run: just bindings-check
```

- [ ] **Step 5: commit**

```bash
git add yi-agent-rs/justfile .github/workflows/ci.yml
git commit -m "ci: fail when the ts-rs bindings drift from the Rust protocol"
```
