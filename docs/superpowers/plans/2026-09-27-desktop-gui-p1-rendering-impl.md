# Desktop GUI P1 后半 — Markdown 渲染 + 用量/成本面板 实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (or
> superpowers:subagent-driven-development) to implement this plan task-by-task.

**Goal:** 把桌面 GUI 的 agent 消息从纯文本渲染升级为 markdown + 代码高亮,并把
token 用量扩展为含 cache 的用量/成本面板。

**Architecture:** 纯前端渲染改动(markdown/高亮/面板)+ 一处协议字段扩展
(`thread/tokenUsage/updated` 补 cache token,从 core 的 `TokenUsage` 一路透传到
前端)。不改 agent 行为,不新增 item 类型。

**Tech Stack:** React 19 + TypeScript + Tailwind CSS v4 + Vite, react-markdown /
remark-gfm / rehype-highlight, Vitest(+ jsdom,文件级), Tauri 2。

**Design reference:** `docs/superpowers/specs/2026-09-27-desktop-gui-p1-rendering-design.md`

**前置:** `feat/desktop-gui-p1-persistence` 已合入 main。

---

## Global constraints (read before starting)

- 在 worktree `.worktrees/feat/desktop-gui-p1-rendering` 里工作,**绝不**在 `main` 提交。
- Commit 风格:conventional commits,首行 ≤72 字符,**不写** `Co-Authored-By`。
- Rust 改动提交前先 `cd yi-agent-rs && cargo fmt --all`。
- **跑 cargo 前先 `ps aux | grep cargo` 确认没有其它 cargo 进程**(其它 worktree
  可能正在跑)。同时只有一个 cargo 进程。
- 不跑 `cargo test --workspace`;按 crate 跑 `cargo test -p yi-agent-app-server`。
- 前端:`cd desktop && npx vitest run` / `npm run build`。
- `desktop/` 不是 cargo workspace 成员;`desktop/src-tauri` 是独立 cargo 项目。
- 现有测试若因本次改动而失败(如 session.test.ts 的 usage 断言),**更新**它,不要删。

### 关键事实(已核对)

- core `TokenUsage` 的 cache 字段是 `Option<u32>`(`yi-agent-core/src/provider.rs:37-38`),
  且该结构**只 derive Serialize**。翻译层需 `.unwrap_or(0)`。
- `protocol::Notification` **只 derive Serialize**(protocol.rs:105);其 `TokenUsage`
  变体不需要 `#[serde(default)]`,直接总是序列化新字段。
- `thread_store::TurnUsage` derive Serialize + Deserialize(thread_store.rs:28),
  旧 `.jsonl` 兼容**需要** `#[serde(default)]`。
- `Notification::TokenUsage` 构造点仅 2 处:translate.rs:237、server.rs:444。
- `TurnUsage` 构造点仅 1 处:server.rs:898。
- translate 的 `usage_emits_token_usage` 测试用 `..` 匹配,加字段不会破坏它。
- ChatView 的 `lastText` 依赖 `item.type === "agentMessage" ? item.text`,保持不变。

---

## Task 1: 依赖 + Tailwind typography + Tauri opener 装配

**Files:**
- Modify: `desktop/package.json`(+ `package-lock.json`)
- Modify: `desktop/src/index.css`
- Modify: `desktop/src-tauri/Cargo.toml`
- Modify: `desktop/src-tauri/src/lib.rs`
- Modify: `desktop/src-tauri/capabilities/default.json`

**Step 1: 安装前端依赖**

```bash
cd desktop
npm install react-markdown remark-gfm rehype-highlight highlight.js @tauri-apps/plugin-opener
npm install -D @tailwindcss/typography jsdom @testing-library/react
```

**Step 2: 接入 Tailwind typography 插件**

`desktop/src/index.css` 当前只有一行 `@import "tailwindcss";`。在其后追加:

```css
@import "tailwindcss";
@plugin "@tailwindcss/typography";
```

**Step 3: 加 Tauri opener 插件(Rust 侧)**

`desktop/src-tauri/Cargo.toml` 的 `[dependencies]` 追加:

```toml
tauri-plugin-opener = "2"
```

`desktop/src-tauri/src/lib.rs` 在 shell 插件后注册(第 6 行 `.plugin(...)` 之后):

```rust
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
```

`desktop/src-tauri/capabilities/default.json` 的 `permissions` 数组追加:

```json
    "core:default",
    "opener:default"
```

**Step 4: 验证**

```bash
cd desktop && npm run build
cd desktop/src-tauri && cargo check
```

Expected:`npm run build` 通过;`cargo check` 通过(首次会编译 tauri,较慢)。

**Step 5: Commit**

```bash
git add desktop/package.json desktop/package-lock.json desktop/src/index.css \
  desktop/src-tauri/Cargo.toml desktop/src-tauri/src/lib.rs \
  desktop/src-tauri/capabilities/default.json
git commit -m "feat(desktop): add markdown/typography deps and opener plugin"
```

> 注:若 `cargo check` 因 tauri build script 需要 `frontendDist` 报错,先确保
> `cd desktop && npm run build` 已生成 `dist/`。

---

## Task 2: `TurnUsage` 补 cache 字段 + driver 落盘

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:27-33`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:897-903`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`(tests 模块)

**Step 1: 写失败测试**

在 `thread_store.rs` 的 `#[cfg(test)] mod tests` 内追加(按该文件已有测试风格,
用 `serde_json::from_str` 构造行):

```rust
    #[test]
    fn turn_usage_loads_without_cache_fields() {
        // 旧格式:没有 cache 字段的 usage 行仍须能加载,缺失字段按 0。
        let line = r#"{"type":"turn","items":[],"usage":{"model":"m","input_tokens":7,"output_tokens":2}}"#;
        let parsed: TurnLine = serde_json::from_str(line).unwrap();
        let TurnLine::Turn { usage, .. } = parsed;
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 2);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn turn_usage_round_trips_cache_fields() {
        let u = TurnUsage {
            model: "m".into(),
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 30,
            cache_read_input_tokens: 40,
        };
        let s = serde_json::to_string(&u).unwrap();
        let back: TurnUsage = serde_json::from_str(&s).unwrap();
        assert_eq!(back.cache_creation_input_tokens, 30);
        assert_eq!(back.cache_read_input_tokens, 40);
    }
```

**Step 2: 运行确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib turn_usage
```

Expected:编译失败(字段 `cache_creation_input_tokens` 不存在)——即红。

**Step 3: 实现**

`thread_store.rs` 的 `TurnUsage` 改为:

```rust
/// 一次 turn 的 token 用量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 写入 prompt cache 的 token(Anthropic cache write);旧日志缺失按 0。
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    /// 命中 prompt cache 读取的 token;旧日志缺失按 0。
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}
```

`server.rs:897-903` 的 driver 构造改为:

```rust
                            if let yi_agent_core::AgentEvent::Usage { model, usage } = &e {
                                last_usage = Some(crate::thread_store::TurnUsage {
                                    model: model.clone(),
                                    input_tokens: usage.input_tokens,
                                    output_tokens: usage.output_tokens,
                                    cache_creation_input_tokens: usage
                                        .cache_creation_input_tokens
                                        .unwrap_or(0),
                                    cache_read_input_tokens: usage
                                        .cache_read_input_tokens
                                        .unwrap_or(0),
                                });
                            }
```

**Step 4: 运行确认通过**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-app-server --lib
```

Expected:PASS(全部 lib 测试,含新增 2 个)。

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs \
  yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): persist cache tokens in turn usage"
```

---

## Task 3: `Notification::TokenUsage` 补 cache 字段 + translate + resume 回放

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:143-149`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:236-243`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:441-452`
- Test: `protocol.rs` tests, `translate.rs` tests

**Step 1: 写失败测试**

`protocol.rs` tests 模块追加:

```rust
    #[test]
    fn token_usage_notification_includes_cache_fields() {
        let n = Notification::TokenUsage {
            thread_id: "t1".into(),
            model: "m".into(),
            input_tokens: 10,
            output_tokens: 3,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 200,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/tokenUsage/updated");
        assert_eq!(v["params"]["input_tokens"], 10);
        assert_eq!(v["params"]["output_tokens"], 3);
        assert_eq!(v["params"]["cache_creation_input_tokens"], 100);
        assert_eq!(v["params"]["cache_read_input_tokens"], 200);
    }
```

`translate.rs` tests 模块追加(现有 `translator()` helper 可复用):

```rust
    #[test]
    fn usage_carries_cache_tokens() {
        let mut t = translator();
        let usage = TokenUsage {
            input_tokens: 3,
            output_tokens: 5,
            cache_creation_input_tokens: Some(100),
            cache_read_input_tokens: Some(200),
        };
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage,
        });
        match &out[0] {
            Notification::TokenUsage {
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                assert_eq!(*cache_creation_input_tokens, 100);
                assert_eq!(*cache_read_input_tokens, 200);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn usage_missing_cache_tokens_defaults_to_zero() {
        let mut t = translator();
        let usage = TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        };
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage,
        });
        match &out[0] {
            Notification::TokenUsage {
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                assert_eq!(*cache_creation_input_tokens, 0);
                assert_eq!(*cache_read_input_tokens, 0);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }
```

**Step 2: 运行确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib token_usage
```

Expected:编译失败(字段不存在)。

**Step 3: 实现**

`protocol.rs` 的 `TokenUsage` 变体:

```rust
    #[serde(rename = "thread/tokenUsage/updated")]
    TokenUsage {
        thread_id: String,
        model: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
    },
```

`translate.rs:236-243` 的 `AgentEvent::Usage` 分支:

```rust
            AgentEvent::Usage { model, usage } => {
                out.push(Notification::TokenUsage {
                    thread_id: self.thread_id.clone(),
                    model,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_creation_input_tokens: usage
                        .cache_creation_input_tokens
                        .unwrap_or(0),
                    cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                });
            }
```

`server.rs:441-452` 的 resume 回放:

```rust
                        if let Some(u) = loaded.usage {
                            write_notification(
                                &writer,
                                &Notification::TokenUsage {
                                    thread_id: thread_id.clone(),
                                    model: u.model,
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                                    cache_read_input_tokens: u.cache_read_input_tokens,
                                },
                            )
                            .await?;
                        }
```

**Step 4: 运行确认通过**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-app-server
```

Expected:PASS(全部测试)。

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs \
  yi-agent-rs/crates/yi-agent-app-server/src/translate.rs \
  yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): carry cache tokens on tokenUsage notifications"
```

---

## Task 4: 前端协议类型 + session 用量映射

**Files:**
- Modify: `desktop/src/lib/protocol.ts:61-64`(Notification 的 tokenUsage params)+ 新增 `Usage` 接口
- Modify: `desktop/src/lib/session.ts:19, 84-90`
- Test: `desktop/src/lib/session.test.ts`

**Step 1: 写失败测试**

`desktop/src/lib/session.test.ts`:更新已有的 "records token usage" 测试并新增一个。
把原测试改为(加 cache 字段与断言):

```ts
  it("records token usage", () => {
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t",
        model: "m",
        input_tokens: 10,
        output_tokens: 3,
        cache_creation_input_tokens: 100,
        cache_read_input_tokens: 200,
      },
    });
    expect(s.usage).toEqual({
      model: "m",
      input: 10,
      output: 3,
      cacheWrite: 100,
      cacheRead: 200,
    });
  });

  it("defaults cache tokens to zero when absent", () => {
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: { thread_id: "t", model: "m", input_tokens: 1, output_tokens: 2 },
    });
    expect(s.usage).toEqual({
      model: "m",
      input: 1,
      output: 2,
      cacheRead: 0,
      cacheWrite: 0,
    });
  });
```

**Step 2: 运行确认失败**

```bash
cd desktop && npx vitest run src/lib/session.test.ts
```

Expected:FAIL(`cacheRead`/`cacheWrite` 为 undefined / 类型不匹配)。

**Step 3: 实现**

`desktop/src/lib/protocol.ts`:在文件末尾追加:

```ts
/** Token 用量(前端归一化后)。`cacheWrite` = 写入 cache,`cacheRead` = 命中 cache。 */
export interface Usage {
  model: string;
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}
```

并把 Notification 的 tokenUsage 分支 params 改为:

```ts
  | {
      method: "thread/tokenUsage/updated";
      params: {
        thread_id: string;
        model: string;
        input_tokens: number;
        output_tokens: number;
        /** 缺省(旧服务端)按 0 处理。 */
        cache_creation_input_tokens?: number;
        cache_read_input_tokens?: number;
      };
    }
```

`desktop/src/lib/session.ts`:

- 顶部 import 增加 `Usage`:
  ```ts
  import type { Item, Notification, RetryCause, TurnStatus, Usage } from "./protocol";
  ```
- 字段类型改为:
  ```ts
    usage: Usage | null = null;
  ```
- tokenUsage 分支改为:
  ```ts
      case "thread/tokenUsage/updated":
        this.usage = {
          model: notification.params.model,
          input: notification.params.input_tokens,
          output: notification.params.output_tokens,
          cacheWrite: notification.params.cache_creation_input_tokens ?? 0,
          cacheRead: notification.params.cache_read_input_tokens ?? 0,
        };
        break;
  ```

**Step 4: 运行确认通过**

```bash
cd desktop && npx vitest run src/lib/session.test.ts && npm run build
```

Expected:PASS;`tsc` 通过。

**Step 5: Commit**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "feat(desktop): normalize cache tokens into session usage"
```

---

## Task 5: `pricing.ts` 成本估算

**Files:**
- Create: `desktop/src/lib/pricing.ts`
- Test: `desktop/src/lib/pricing.test.ts`

**Step 1: 写失败测试**

`desktop/src/lib/pricing.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { estimateCost, formatCost, priceFor } from "./pricing";

const usage = (over: Partial<Parameters<typeof estimateCost>[0]>) => ({
  model: "claude-sonnet-4-20250514",
  input: 0,
  output: 0,
  cacheRead: 0,
  cacheWrite: 0,
  ...over,
});

describe("priceFor", () => {
  it("matches by longest prefix", () => {
    // gpt-4o-mini 必须匹配 gpt-4o-mini 而不是 gpt-4o
    expect(priceFor("gpt-4o-mini")?.input).toBe(0.15);
    expect(priceFor("gpt-4o")?.input).toBe(2.5);
  });

  it("returns null for unknown models", () => {
    expect(priceFor("llama-3")).toBeNull();
  });
});

describe("estimateCost", () => {
  it("weights all four token kinds", () => {
    // sonnet-4: input 3, output 15, cacheRead 0.3, cacheWrite 3.75 (USD/1M)
    const cost = estimateCost(
      usage({ input: 1_000_000, output: 1_000_000, cacheRead: 1_000_000, cacheWrite: 1_000_000 }),
    );
    expect(cost).toBeCloseTo(3 + 15 + 0.3 + 3.75, 6);
  });

  it("returns null for unknown models", () => {
    expect(estimateCost(usage({ model: "llama-3" }))).toBeNull();
  });
});

describe("formatCost", () => {
  it("shows em dash for null", () => {
    expect(formatCost(null)).toBe("—");
  });
  it("shows < $0.01 for tiny non-zero costs", () => {
    expect(formatCost(0.0001)).toBe("< $0.01");
  });
  it("shows four decimals otherwise", () => {
    expect(formatCost(0.0123)).toBe("$0.0123");
  });
});
```

**Step 2: 运行确认失败**

```bash
cd desktop && npx vitest run src/lib/pricing.test.ts
```

Expected:FAIL(模块不存在)。

**Step 3: 实现**

`desktop/src/lib/pricing.ts`:

```ts
import type { Usage } from "./protocol";

/** 单价:USD / 1M tokens。 */
export interface Price {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/**
 * 模型 id 前缀 → 单价。按最长前缀匹配。
 *
 * 单价为官方公开定价(USD / 1M tokens),核对日期 2026-09-27。官方调价后需手动更新。
 * OpenAI 的自动 prompt caching 不额外计费写入,故 cacheWrite = 0。
 */
const PRICES: Array<[prefix: string, price: Price]> = [
  ["claude-opus-4", { input: 15, output: 75, cacheRead: 1.5, cacheWrite: 18.75 }],
  ["claude-sonnet-4", { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 }],
  ["claude-3-5-sonnet", { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 }],
  ["claude-3-5-haiku", { input: 0.8, output: 4, cacheRead: 0.08, cacheWrite: 1 }],
  ["gpt-4o-mini", { input: 0.15, output: 0.6, cacheRead: 0.075, cacheWrite: 0 }],
  ["gpt-4o", { input: 2.5, output: 10, cacheRead: 1.25, cacheWrite: 0 }],
  ["gpt-4.1", { input: 2, output: 8, cacheRead: 0.5, cacheWrite: 0 }],
  ["o3", { input: 10, output: 40, cacheRead: 2.5, cacheWrite: 0 }],
  ["o1", { input: 15, output: 60, cacheRead: 7.5, cacheWrite: 0 }],
];

/** 模型对应的单价;未知模型返回 null。最长前缀优先。 */
export function priceFor(model: string): Price | null {
  let best: { prefix: string; price: Price } | null = null;
  for (const [prefix, price] of PRICES) {
    if (model.startsWith(prefix) && (!best || prefix.length > best.prefix.length)) {
      best = { prefix, price };
    }
  }
  return best ? best.price : null;
}

/** 估算成本(USD);未知模型返回 null。 */
export function estimateCost(u: Usage): number | null {
  const p = priceFor(u.model);
  if (!p) return null;
  return (
    (u.input * p.input +
      u.output * p.output +
      u.cacheRead * p.cacheRead +
      u.cacheWrite * p.cacheWrite) /
    1_000_000
  );
}

/** 成本展示:未知 → "—";非零但 < $0.01 → "< $0.01";否则 4 位小数。 */
export function formatCost(cost: number | null): string {
  if (cost === null) return "—";
  if (cost > 0 && cost < 0.01) return "< $0.01";
  return `$${cost.toFixed(4)}`;
}
```

**Step 4: 运行确认通过**

```bash
cd desktop && npx vitest run src/lib/pricing.test.ts
```

Expected:PASS。

**Step 5: Commit**

```bash
git add desktop/src/lib/pricing.ts desktop/src/lib/pricing.test.ts
git commit -m "feat(desktop): add model pricing and cost estimation"
```

---

## Task 6: `UsagePanel` + `StatusBar` 接线

**Files:**
- Create: `desktop/src/components/UsagePanel.tsx`
- Modify: `desktop/src/components/StatusBar.tsx`

**Step 1: 实现 `UsagePanel`**

`desktop/src/components/UsagePanel.tsx`:

```tsx
import { estimateCost, formatCost } from "../lib/pricing";
import type { Usage } from "../lib/protocol";

/** StatusBar 用量区展开后的明细浮层:四类 token + 估算成本。 */
export function UsagePanel({ usage }: { usage: Usage }) {
  return (
    <div className="absolute right-4 top-full z-10 mt-1 w-64 rounded-md border border-neutral-700 bg-neutral-900 p-3 text-xs shadow-lg">
      <div className="mb-2 font-semibold text-neutral-200">Usage</div>
      <dl className="grid grid-cols-2 gap-y-1 font-mono">
        <dt className="text-neutral-400">Input</dt>
        <dd className="text-right">{usage.input} tokens</dd>
        <dt className="text-neutral-400">Output</dt>
        <dd className="text-right">{usage.output} tokens</dd>
        <dt className="text-neutral-400">Cache read</dt>
        <dd className="text-right">{usage.cacheRead} tokens</dd>
        <dt className="text-neutral-400">Cache write</dt>
        <dd className="text-right">{usage.cacheWrite} tokens</dd>
        <dt className="text-neutral-400">Model</dt>
        <dd className="truncate text-right" title={usage.model}>
          {usage.model}
        </dd>
        <dt className="text-neutral-400">Est. cost</dt>
        <dd className="text-right">{formatCost(estimateCost(usage))}</dd>
      </dl>
    </div>
  );
}
```

**Step 2: 接线 `StatusBar`**

`desktop/src/components/StatusBar.tsx` 整体替换为:

```tsx
import { useState } from "react";
import type { Usage } from "../lib/protocol";
import { UsagePanel } from "./UsagePanel";

export function StatusBar({
  cwd,
  model,
  status,
  usage,
}: {
  cwd: string | null;
  model: string | null;
  status: string;
  usage: Usage | null;
}) {
  const connected = status === "connected";
  const [showUsage, setShowUsage] = useState(false);
  return (
    <div className="relative flex items-center gap-3 border-b border-neutral-800 bg-neutral-900 px-4 py-2 text-xs text-neutral-400">
      <span
        className={`inline-block h-2 w-2 rounded-full ${connected ? "bg-emerald-500" : "bg-red-500"}`}
        title={status}
      />
      <span className="text-neutral-300">{status}</span>
      {cwd && <span className="truncate font-mono">{cwd}</span>}
      {model && <span className="truncate font-mono">{model}</span>}
      {usage && (
        <button
          type="button"
          className="ml-auto font-mono hover:text-neutral-200"
          onClick={() => setShowUsage((v) => !v)}
        >
          {usage.input} in / {usage.output} out
        </button>
      )}
      {usage && showUsage && <UsagePanel usage={usage} />}
    </div>
  );
}
```

**Step 3: 验证**

```bash
cd desktop && npm run build
```

Expected:`tsc` + vite 通过(无类型错误)。

**Step 4: Commit**

```bash
git add desktop/src/components/UsagePanel.tsx desktop/src/components/StatusBar.tsx
git commit -m "feat(desktop): add expandable usage/cost panel to status bar"
```

---

## Task 7: `MarkdownText` + ChatView 接线

**Files:**
- Create: `desktop/src/components/MarkdownText.tsx`
- Modify: `desktop/src/components/ChatView.tsx:41-49`
- Test: `desktop/src/components/MarkdownText.test.tsx`

**Step 1: 写失败测试**

`desktop/src/components/MarkdownText.test.tsx`(文件头注释启用 jsdom):

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { MarkdownText } from "./MarkdownText";

describe("MarkdownText", () => {
  it("renders a fenced code block with highlight classes", () => {
    const { container } = render(<MarkdownText text={"```js\nconst x = 1;\n```"} />);
    const code = container.querySelector("pre code");
    expect(code).not.toBeNull();
    expect(code!.className).toContain("hljs");
  });

  it("renders a GFM table", () => {
    const { container } = render(<MarkdownText text={"| a | b |\n| - | - |\n| 1 | 2 |"} />);
    expect(container.querySelector("table")).not.toBeNull();
  });

  it("does not render raw HTML (XSS guard)", () => {
    const { container } = render(
      <MarkdownText text={"before <script>window.__x = 1</script> after"} />,
    );
    expect(container.querySelector("script")).toBeNull();
  });
});
```

**Step 2: 运行确认失败**

```bash
cd desktop && npx vitest run src/components/MarkdownText.test.tsx
```

Expected:FAIL(模块不存在)。

**Step 3: 实现**

`desktop/src/components/MarkdownText.tsx`:

```tsx
import { memo, type MouseEvent, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { open } from "@tauri-apps/plugin-opener";
import "highlight.js/styles/github-dark.css";

/**
 * 外链在系统浏览器打开,避免 webview 被导航走。
 * 非 Tauri 环境(vitest / 浏览器开发)下 `open` 会 reject,吞掉即可。
 */
function ExternalLink({ href, children }: { href?: string; children?: ReactNode }) {
  const onClick = (e: MouseEvent<HTMLAnchorElement>) => {
    if (!href || !/^https?:/i.test(href)) return;
    e.preventDefault();
    void open(href).catch(() => {});
  };
  return (
    <a href={href} onClick={onClick} rel="noreferrer">
      {children}
    </a>
  );
}

/**
 * agentMessage 的 markdown 渲染。按 `text` 记忆:流式时只有正在增长的那条消息
 * 重解析,历史消息命中 memo 不重渲染。
 *
 * 不启用 rehype-raw —— react-markdown 默认丢弃原始 HTML,天然防 XSS。
 */
export const MarkdownText = memo(
  function MarkdownText({ text }: { text: string }) {
    return (
      <div className="prose prose-invert max-w-none prose-pre:bg-neutral-900">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          rehypePlugins={[rehypeHighlight]}
          components={{ a: ExternalLink }}
        >
          {text}
        </ReactMarkdown>
      </div>
    );
  },
  (prev, next) => prev.text === next.text,
);
```

`desktop/src/components/ChatView.tsx` 的 `agentMessage` 分支(第 41-49 行)改为:

```tsx
          case "agentMessage":
            return (
              <div key={item.id} className="my-1 max-w-[90%] self-start">
                <MarkdownText text={item.text} />
              </div>
            );
```

并在文件顶部 import 增加:

```tsx
import { MarkdownText } from "./MarkdownText";
```

**Step 4: 运行确认通过**

```bash
cd desktop && npx vitest run && npm run build
```

Expected:全部前端测试 PASS(含 3 个新组件测试);`tsc` 通过。

**Step 5: Commit**

```bash
git add desktop/src/components/MarkdownText.tsx \
  desktop/src/components/MarkdownText.test.tsx \
  desktop/src/components/ChatView.tsx
git commit -m "feat(desktop): render agent messages as markdown with highlighting"
```

---

## Task 8: 文档同步

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/README.md`
- Modify: `desktop/README.md`

**Step 1: 采集真实数字**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server 2>&1 | grep "test result" | head -1
cd desktop && npx vitest run 2>&1 | grep -E "Test Files|Tests "
```

记录:app-server 测试总数、前端测试总数与各文件分布。

**Step 2: 更新 `desktop.md`**

- "范围边界 / 做什么" 追加一条:
  `- agentMessage markdown 富渲染 + 代码高亮(react-markdown + remark-gfm + rehype-highlight)`
  `- 用量/成本面板(StatusBar 可展开:input/output/cache + 估算成本)`
- "不做什么" 删除 `- 不做 markdown 富渲染 / 代码高亮` 一行。
- Features 追加一条 `[x]`:
  `- [x] agentMessage markdown 富渲染 + 代码高亮 + 用量/成本面板 — desktop/src/components/MarkdownText.tsx:1（memo + remark-gfm + rehype-highlight）/ desktop/src/components/ChatView.tsx:41（接线）/ desktop/src/lib/pricing.ts:1（定价表 + estimateCost）/ desktop/src/components/UsagePanel.tsx:1 / desktop/src/components/StatusBar.tsx:14（点击展开）`
- 修正 `模块说明` 里 app-server 测试数(当前写 103,以 Step 1 实测为准)。
- 修正 `验证命令` 行的前端测试数与分布(以 Step 1 实测为准)。

**Step 3: 更新 `yi-agent-app-server.md`**

在会话持久化/用量相关 feature 处补一句:`thread/tokenUsage/updated` 现携带
`cache_creation_input_tokens` / `cache_read_input_tokens`(`translate.rs` 透传,
`thread_store.rs` 的 `TurnUsage` 落盘并 `#[serde(default)]` 兼容旧日志);
引用 `protocol.rs:143`。

**Step 4: 更新 `README.md` 计数**

`desktop | 11 / 12` → `desktop | 12 / 12`(新增一条 `[x]`)。核对 `desktop.md`
的 `[x]` 与 `[ ]` 数量与计数一致。

**Step 5: 更新 `desktop/README.md`**

把测试数量描述更新为 Step 1 实测值,并在功能列表补一句 markdown 渲染 + 用量面板。

**Step 6: Commit**

```bash
git add docs/project-management/desktop.md docs/project-management/yi-agent-app-server.md \
  docs/project-management/README.md desktop/README.md
git commit -m "docs: sync desktop rendering + usage panel progress"
```

---

## Task 9: 最终验证

**Step 1: 确认没有并发 cargo**

```bash
ps aux | grep -v grep | grep -E "cargo|rustc" | head
```

若为空,继续。

**Step 2: 跑全部相关测试**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-app-server
cd desktop && npx vitest run && npm run build
```

Expected:app-server 全绿;前端全绿;build 通过。

**Step 3: 确认无遗留冲突标记 / 未提交改动**

```bash
git status --short
grep -rn "^<<<<<<<" desktop/src yi-agent-rs/crates/yi-agent-app-server/src || true
```

Expected:无冲突标记;`git status` 只剩预期内容。

**Step 4: 手工冒烟(可选,人工)**

```bash
cd desktop && npm run sidecar && npm run tauri dev
```

逐项确认:markdown 标题/列表/表格渲染、代码块高亮、点外链在系统浏览器打开、
展开用量面板看到 token 明细与成本。

---

## 完成判据

- `cargo test -p yi-agent-app-server` 全绿。
- `cd desktop && npx vitest run` 全绿;`npm run build` 通过。
- agent 消息渲染为 markdown,代码块有 `hljs` 高亮;原始 HTML 不渲染。
- StatusBar 用量区可展开,显示四类 token + 估算成本,未知模型显示 `—`。
- 文档计数与实测一致。
