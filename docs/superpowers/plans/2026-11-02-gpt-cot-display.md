# GPT 思考块空白修复 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Responses 通道（GPT 系）的「思考」块在拿到摘要时显示摘要、没拿到时显示「已推理（无明文）」占位，而不是一片空白。

**Architecture:** 两处独立缺陷分别修：后端在 `responses/stream.rs` 里把上游 `item.summary[].text` 拼成展示文本填入 `ThinkingBlock.text`（此前恒为空串）；前端在 `session.ts` 的三处「按 id 替换」处加守卫，不让空文本覆盖已流式收到的摘要；最后在 `ChatView` 的 `ReasoningBlock` 做三态降级渲染。请求侧完全不动。

**Tech Stack:** Rust（`yi-agent-llm` crate，tokio + serde_json + SSE 自研解析器）、TypeScript + React 19、Vitest。

**Spec:** `docs/superpowers/specs/2026-11-02-gpt-cot-display-design.md`

## Global Constraints

- 工作目录：`/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display`（分支 `feat/gpt-cot-display`）。**不要**在 `main` 上改任何文件。
- 请求侧保持原样：**不得**修改 `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs` 的 `reasoning` 构造，**不得**动 `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs` 的 `unset_reasoning_effort_stays_none`（spec 选项 1）。
- 凭证逐字保真：`ThinkingToken::Encrypted { encrypted_content, summary }` 的 `summary` 必须保留上游原文（含 `type` 字段与分段结构），**不得**被新加的展示文本改写。
- 占位文案固定为 `已推理（无明文）`，与 `已加密的思考` 并列；不要改动「思考」标题与 `text-fg-subtle` 样式。
- `data-testid="reasoning-block"` 必须保留（既有测试依赖）。
- Rust 侧命令统一在 `yi-agent-rs/` 下执行；前端命令统一在 `desktop/` 下执行，且该目录**没有** `node_modules`，需先 `npm ci`（见 Task 0）。
- 提交信息用 conventional commits，首行 ≤72 字符，**不要**写 `Co-Authored-By`。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。

---

### Task 0: 准备 worktree 的构建环境

worktree 是新建的，`desktop/` 没有 `node_modules`（软链 `node_modules` 会让 vite 解析失败——已实测 `ERR_MODULE_NOT_FOUND: Cannot find package 'vite'`），`yi-agent-rs/` 没有 `target`。

**Files:** 无代码改动。

**Interfaces:**
- Consumes: 无
- Produces: 可运行的 `npx vitest` 与 `cargo test -p yi-agent-llm`

- [ ] **Step 1: 安装前端依赖**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npm ci
```

Expected: 结束无 error；`ls -d node_modules` 存在且**不是**符号链接。

- [ ] **Step 2: 验证前端基线可跑**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/lib/session.test.ts
```

Expected: `Tests 80 passed (80)`（这是本计划改动前已有的基线；同目录
`src/components/ChatView.test.tsx` 的基线是 53，见 Task 3）。

- [ ] **Step 3: 编译 Rust 侧目标 crate（首次较慢）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/yi-agent-rs
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-llm --lib --no-run
```

Expected: 编译通过。若报 `Blocking waiting for file lock`，按项目规范等待或检查是否有被 `SIGSTOP` 挂起的 cargo 进程（`ps ax -o pid=,stat=,command= | grep -E "cargo|rustc"`），不要删 lock 文件。

> 实测记录（2026-11-02）：本机 `cargo` 不在默认 PATH，必须按上面的 `export` 补
> `$HOME/.cargo/bin`；首次 `--no-run` 约 15 秒（增量目标 crate）。

- [ ] **Step 4: 无需提交**（本任务不产生代码变更）

---

### Task 1: 后端把 summary 文本带进 thinking 块（TDD）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs:158-170`（`response.output_item.done` 的 reasoning 分支里构造 `ThinkingBlock` 的那段）
- Test: 同文件 `mod tests`（追加到 `reasoning_item_done_captures_an_empty_summary_as_an_empty_vec` 之后，约 `:592`）

**Interfaces:**
- Consumes: 无（只改 crate 内部）
- Produces: `ProviderEvent::ThinkingBlock { text: String, token: ThinkingToken }` 中 `text` 不再恒为空；`token` 内 `summary` 保持上游原文。Task 3 的测试依赖这一点（上游已由 `translate.rs` 透传，无需改动）。

- [ ] **Step 1: 写两个失败测试**

> 位置说明：`stream.rs` 的 `mod tests` 有**两处** `// ---- 以下为超出 brief 的防御性用例 ----`
> 注释（一处约 `:418` 在 request 测试组，一处约 `:598` 在 response 解析测试组）。
> 新用例必须插在**后者**（紧跟 `empty_summary_yields...` 之前的那一处），否则会落进
> request 测试组。定位锚点用：`assert!(found, "empty summary must be captured as an empty vec");`
> 之后的那条注释。

在 `stream.rs` 的 `mod tests` 里、`reasoning_item_done_captures_an_empty_summary_as_an_empty_vec` 函数之后追加：

```rust
    #[tokio::test]
    async fn reasoning_item_summary_text_is_carried_into_the_thinking_block() {
        // 上游 reasoning item 的 summary[] 是展示用明文。此前的实现把
        // `ThinkingBlock.text` 写死成空串，导致前端接管后「思考」块空白。
        // 这里钉住：text = 各段 text 的拼接；而 token.summary 仍是原始结构。
        let body = "event: response.output_item.done\n\
             data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-blob\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thought A\"},{\"type\":\"summary_text\",\"text\":\" B\"}]}}\n\n\
             event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events: Vec<ProviderEvent> = collect_chunks(&[bytes.as_slice()])
            .await
            .into_iter()
            .collect();

        let (text, token) = events
            .iter()
            .find_map(|e| match e {
                ProviderEvent::ThinkingBlock { text, token } => Some((text.clone(), token.clone())),
                _ => None,
            })
            .expect("a thinking block must be emitted");

        assert_eq!(text, "thought A B", "summary 各段文本须按序拼成展示文本");
        assert_eq!(
            token,
            yi_agent_core::ThinkingToken::Encrypted {
                encrypted_content: "enc-blob".into(),
                // 原始结构逐字保留：两段、带 type 字段，未被拼平或改造。
                summary: vec![
                    serde_json::json!({"type": "summary_text", "text": "thought A"}),
                    serde_json::json!({"type": "summary_text", "text": " B"}),
                ],
            },
            "凭证里的 summary 必须保持上游原文"
        );
    }

    #[tokio::test]
    async fn empty_summary_yields_an_empty_thinking_block_text() {
        // 上游返回 summary: [] 时（实测 Sol 在部分轮次如此），展示文本为空，
        // 由渲染层降级为「已推理（无明文）」。凭证仍必须带上空数组以支持回放。
        let body = "event: response.output_item.done\n\
             data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-blob\",\"summary\":[]}}\n\n\
             event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events: Vec<ProviderEvent> = collect_chunks(&[bytes.as_slice()])
            .await
            .into_iter()
            .collect();

        let text = events
            .iter()
            .find_map(|e| match e {
                ProviderEvent::ThinkingBlock { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a thinking block must be emitted");
        assert_eq!(text, "", "空 summary must degrade to an empty display text");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/yi-agent-rs
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-llm --lib reasoning_item_summary_text_is_carried_into_the_thinking_block
```

Expected: FAIL，断言消息形如
`assertion `left == right` failed: summary 各段文本须按序拼成展示文本`，left 是 `""`、right 是 `"thought A B"`。

（第二条 `empty_summary_yields_an_empty_thinking_block_text` 此刻会**通过**，因为当前实现恰好就产出空串。它是回归护栏，不是驱动实现的红灯——先跑一次确认它绿，再进入 Step 3。）

- [ ] **Step 3: 实现最小改动**

把 `stream.rs:158-170` 的这段：

```rust
                            events.push(ProviderEvent::ThinkingBlock {
                                text: String::new(),
                                token: yi_agent_core::ThinkingToken::Encrypted {
                                    encrypted_content: enc.to_string(),
                                    // 逐字带走上游 `summary`（缺失/非数组时落为空 Vec），
                                    // 供下一轮回放——回放时该字段必须显式在场。
                                    summary: item
                                        .get("summary")
                                        .and_then(Value::as_array)
                                        .cloned()
                                        .unwrap_or_default(),
                                },
                            });
```

替换为：

```rust
                            // 逐字带走上游 `summary`（缺失/非数组时落为空 Vec），
                            // 供下一轮回放——回放时该字段必须显式在场。
                            let summary: Vec<Value> = item
                                .get("summary")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            // 展示用明文：同一份数组按序拼出各段 `text`。
                            // 只读不改——`summary` 本体必须保持上游原文。
                            let text: String = summary
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect();
                            events.push(ProviderEvent::ThinkingBlock {
                                text,
                                token: yi_agent_core::ThinkingToken::Encrypted {
                                    encrypted_content: enc.to_string(),
                                    summary,
                                },
                            });
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/yi-agent-rs
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-llm --lib reasoning_item_summary_text_is_carried_into_the_thinking_block
cargo test -p yi-agent-llm --lib empty_summary_yields_an_empty_thinking_block_text
```

Expected: 两条都 PASS。

- [ ] **Step 5: 跑该 crate 全部测试，确认无回归**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/yi-agent-rs
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-llm
```

Expected: 全绿。特别确认这些既有用例仍通过：
`reasoning_item_done_emits_thinking_block_with_encrypted_content`、
`reasoning_item_done_captures_summary_verbatim`、
`reasoning_item_done_captures_an_empty_summary_as_an_empty_vec`、
`reasoning_item_without_encrypted_content_emits_no_block`。

若 `reasoning_item_done_captures_summary_verbatim` 失败，说明实现把 `summary` 拼平或改写成了别的形状——回到 Step 3 检查是否误改了 `summary`。

- [ ] **Step 6: 格式化并提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/yi-agent-rs
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt --all
cd ..
git add yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs
git commit -m "fix(llm): 把 reasoning summary 文本带进 thinking 块

Responses 通道此前把 ThinkingBlock.text 写死为空串，摘要只走
ReasoningDelta 旁路，导致前端在块级条目接管后「思考」块空白。改为从
上游 item.summary[] 按序拼出展示文本；凭证里的 summary 仍保持原文。"
```

---

### Task 2: 前端三处替换点加「空文本不覆盖」守卫（TDD）

**Files:**
- Modify: `desktop/src/lib/session.ts`（新增私有方法 `mergeThinkingText`；改三处：`:552`、`:563`、`:462`）
- Test: `desktop/src/lib/session.test.ts`（在 `describe("reasoning stream", ...)` 内追加）

**Interfaces:**
- Consumes: Task 1 产出的非空 `text`（但本任务的守卫对「空 / 非空」都成立，不依赖 Task 1 落地）。
- Produces: `private static mergeThinkingText(existing: Item | NoticeItem | ReasoningItem | undefined, incoming: Item): Item`
  —— 两参数：`existing` 是 `this.items` 里当前同 id 的元素（接管那一刻就是本地 `ReasoningItem`，
  它与 `this.openReasoning` 是**同一个对象引用**，所以 `existing.text` 直接就是流式文本，无需额外
  传兜底文本）。**注意** `existing` 必须是联合类型：`items` 的声明类型是
  `(Item | NoticeItem | ReasoningItem)[]`，只写 `Item` 会编译失败。Task 3 不调用它，
  但 Task 3 的用户可见效果依赖它。

- [ ] **Step 1: 写五个失败测试**

在 `desktop/src/lib/session.test.ts` 的 `describe("reasoning stream", () => {` 块内追加（放在 `hands the open local block over to the server's thinking item in place` 之后）：

```ts
    it("keeps the streamed summary when the server thinking item arrives empty", () => {
      // 服务端块级条目不携带明文（摘要走 reasoningDelta 旁路）。用它整体替换
      // 本地块会把已收到的摘要清空——这正是「思考块点开空白」的成因。
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "SUMMARY", kind: "Summary" },
      });
      s.apply({
        method: "item/started",
        params: {
          thread_id: "t",
          item: { type: "thinking", id: "item-1", text: "", redacted: false },
        },
      });
      expect(s.items).toHaveLength(1);
      expect(s.items[0]).toMatchObject({ type: "thinking", id: "item-1", text: "SUMMARY" });
    });

    it("does not clear the summary again on the paired item/completed", () => {
      // 配对帧携带同一个空 text，走「按 id 就地替换」那条分支。只守卫接管处
      // 不足以修好：这里复现第二次清空。
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "SUMMARY", kind: "Summary" },
      });
      const item = { type: "thinking", id: "item-1", text: "", redacted: false } as const;
      s.apply({ method: "item/started", params: { thread_id: "t", item: { ...item } } });
      s.apply({ method: "item/completed", params: { thread_id: "t", item: { ...item } } });
      expect(s.items).toHaveLength(1);
      expect((s.items[0] as { text: string }).text).toBe("SUMMARY");
    });

    it("prefers the server's text when it carries one", () => {
      // 服务端有明文时是权威（GPT 修复后走这条），本地旧文本必须让位。
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "LOCAL", kind: "Content" },
      });
      s.apply({
        method: "item/started",
        params: {
          thread_id: "t",
          item: { type: "thinking", id: "item-1", text: "SERVER", redacted: false },
        },
      });
      expect((s.items[0] as { text: string }).text).toBe("SERVER");
    });

    it("never keeps plaintext once a block is marked redacted", () => {
      // 安全边界：后到帧把块标记为 redacted 时，沿用旧明文等于把本应无明文的
      // 占位变成明文泄漏。
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "SECRET", kind: "Content" },
      });
      s.apply({
        method: "item/started",
        params: {
          thread_id: "t",
          item: { type: "thinking", id: "item-1", text: "", redacted: true },
        },
      });
      expect(s.items[0]).toMatchObject({ type: "thinking", redacted: true, text: "" });
    });
```

再追加一条覆盖 `upsertItems`（回放补帧路径）：

```ts
    it("does not clear existing text when upserting an empty thinking item", () => {
      // `upsertItems` 是回放/补齐的入口，按 id 就地替换。它同样不得用空文本
      // 覆盖已有内容。
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "SUMMARY", kind: "Summary" },
      });
      s.apply({
        method: "item/started",
        params: {
          thread_id: "t",
          item: { type: "thinking", id: "item-1", text: "", redacted: false },
        },
      });
      s.upsertItems([{ type: "thinking", id: "item-1", text: "", redacted: false }]);
      expect((s.items[0] as { text: string }).text).toBe("SUMMARY");
    });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/lib/session.test.ts -t "keeps the streamed summary when the server thinking item arrives empty"
npx vitest run src/lib/session.test.ts -t "does not clear the summary again on the paired item/completed"
```

Expected: 两条都 FAIL，断言 `text` 实际为 `""`。
`prefers the server's text when it carries one` 会**通过**（当前实现就是无条件用服务端），它是护栏。

- [ ] **Step 3: 加 `mergeThinkingText` 私有方法**

在 `desktop/src/lib/session.ts` 的 `private indexOfItem(id: string): number { ... }` 之前插入：

```ts
  /**
   * Fold a server `thinking` item over the text we already hold.
   *
   * 块级 `thinking` 条目不携带明文（摘要走 `thread/reasoningDelta` 旁路），
   * 所以它的 `text` 通常是空串。无条件用它整体替换本地块，会把已经流式收到
   * 的摘要清空——这就是「思考」块点开空白的成因。规则：
   *
   * - 非 thinking 项：原样返回。
   * - `redacted`：原样返回，**不保留任何旧文本**。否则后到帧把块标成加密时，
   *   旧明文会留在本应无明文的占位上。
   * - 服务端带明文：用服务端的（它是权威）。
   * - 服务端空明文：沿用 `existing` 的文本。**`existing` 可能是本地
   *   `ReasoningItem`（接管那一刻就是它，与 `this.openReasoning` 同一对象），
   *   也可能是已接管的 `thinking` 条目，两者都要取 `text`；其它类型不取。
   *
   * `existing` 取 `items` 的声明类型（`Item | NoticeItem | ReasoningItem`）：
   * 接管那一刻它就是本地 `ReasoningItem`，只写 `Item` 会编译失败。
   */
  private static mergeThinkingText(
    existing: Item | NoticeItem | ReasoningItem | undefined,
    incoming: Item,
  ): Item {
    if (incoming.type !== "thinking" || incoming.redacted) return incoming;
    if (incoming.text !== "") return incoming;
    // 接管时 existing 是本地 `reasoning` 块（流式摘要就在它的 text 里）；
    // 配对 completed / upsert 时 existing 是已接管的 `thinking` 条目。
    // 两者都要读，否则接管那一步等于没修。
    const held =
      existing && (existing.type === "thinking" || existing.type === "reasoning")
        ? existing.text
        : "";
    return { ...incoming, text: held };
  }
```

- [ ] **Step 4: 改三处替换点**

**(a)** `apply` 的接管分支（`session.ts:550-557`）——`this.items[held]` 与 `this.openReasoning`
是同一对象，故流式文本由 `existing.text` 带入，无需另传：

```ts
          const held = this.items.findIndex((i) => i.id === this.openReasoning!.id);
          if (held >= 0) {
            this.items[held] = Session.mergeThinkingText(this.items[held], item);
            this.structuralVersion += 1;
            this.lastServerItemId = item.id;
            this.breakReasoningRun();
            break;
          }
```

**(b)** `apply` 的按 id 替换（`session.ts:561-564`）：

```ts
        const index = this.items.findIndex((i) => i.id === item.id);
        if (index >= 0) {
          this.items[index] = Session.mergeThinkingText(this.items[index], item);
          this.structuralVersion += 1;
        } else if (
```

**(c)** `upsertItems` 的按 id 替换（`session.ts:460-463`）：

```ts
      const index = this.items.findIndex((i) => i.id === normalized.id);
      if (index >= 0) {
        this.items[index] = Session.mergeThinkingText(this.items[index], normalized);
        this.structuralVersion += 1;
      } else if (normalized.type === "userMessage") {
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/lib/session.test.ts
```

Expected: 全部通过（80 + 新增 5 = 85）。

- [ ] **Step 6: 类型检查**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npx tsc --noEmit
```

Expected: 无输出（无错误）。

- [ ] **Step 7: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display
git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "fix(desktop): 思考块接管时不被空文本清空

块级 thinking 条目不带明文，三处按 id 替换都会用空串覆盖已流式收到的
摘要（接管处、配对 completed、upsertItems 回放）。统一走
mergeThinkingText：空文本沿用旧值，redacted 时一律不保留明文。"
```

---

### Task 3: 空明文降级为「已推理（无明文）」（TDD）

**Files:**
- Modify: `desktop/src/components/ChatView.tsx:77-82`（`ReasoningBlock` 展开区）
- Test: `desktop/src/components/ChatView.test.tsx`（在 `describe("ChatView reasoning", ...)` 内追加）

**Interfaces:**
- Consumes: `Item::Thinking { text, redacted }`（形状不变，Task 1/2 只影响 `text` 的取值）。
- Produces: 用户可见的三态渲染。无下游依赖。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/components/ChatView.test.tsx` 的 `describe("ChatView reasoning", () => {` 块内追加（放在 `renders a redacted thinking item as a placeholder, not raw text` 之后）：

```tsx
  it("renders an empty non-redacted thinking item as a「已推理（无明文）」placeholder", () => {
    // 模型推理过但本次没有可展示的明文（实测 Sol 部分轮次 summary 为空）。
    // 此前展开后是一片空白。
    const thinking = { type: "thinking", id: "th2", text: "", redacted: false };
    render(<ChatView items={[thinking] as unknown as (Item | NoticeItem)[]} />);
    fireEvent.click(screen.getByRole("button", { name: /思考/ }));
    expect(screen.getByText("已推理（无明文）")).toBeTruthy();
  });

  it("keeps rendering the real summary when one is present", () => {
    // 有明文时不得被占位文案顶掉。
    const thinking = { type: "thinking", id: "th3", text: "真实摘要", redacted: false };
    render(<ChatView items={[thinking] as unknown as (Item | NoticeItem)[]} />);
    fireEvent.click(screen.getByRole("button", { name: /思考/ }));
    expect(screen.getByText("真实摘要")).toBeTruthy();
    expect(screen.queryByText("已推理（无明文）")).toBeNull();
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/components/ChatView.test.tsx -t "renders an empty non-redacted thinking item"
```

Expected: FAIL，`Unable to find an element with the text: 已推理（无明文）`。

- [ ] **Step 3: 实现三态渲染**

把 `ChatView.tsx:77-82` 的：

```tsx
      {expanded && (
        <div className="mt-1 whitespace-pre-wrap">
          {/* redacted 块无明文（凭证不下发）：只呈现「已加密」占位，不显示空白。 */}
          {redacted ? "已加密的思考" : text}
        </div>
      )}
```

替换为：

```tsx
      {expanded && (
        <div className="mt-1 whitespace-pre-wrap">
          {/*
            三态：redacted 块无明文（凭证不下发）→「已加密」；有明文 → 原文；
            模型推理过但本次没有可展示的明文 →「已推理（无明文）」。最后一档
            此前会渲染成一片空白。
          */}
          {redacted ? "已加密的思考" : text !== "" ? text : "已推理（无明文）"}
        </div>
      )}
```

同时更新该组件上方文档注释（`ChatView.tsx:50-59` 那段）里关于占位的描述，把最后一句从

```
 * 「已加密」占位。
```

改为

```
 * 「已加密」占位；明文为空的非 redacted 块呈现「已推理（无明文）」占位。
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/components/ChatView.test.tsx
```

Expected: 全绿，含新增两条与既有的 `renders a dim「思考」block, collapsed by default and expandable`、`renders a replayed server thinking item with the same collapsible card`、`renders a redacted thinking item as a placeholder, not raw text`。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display
git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "feat(desktop): 空明文的思考块降级为「已推理（无明文）」

模型推理过但本轮没有可展示明文时，展开后此前是一片空白。改为三态渲染：
已加密 / 原文 / 已推理（无明文）。"
```

---

### Task 4: 全量验证与文档同步

**Files:**
- Modify: `docs/project-management/desktop.md`（把 `:130` 的 `- [ ] reasoning / thinking 展示` 改为 `- [x]` 并补判据）
- Modify: `docs/project-management/README.md:32`（desktop 计数）
- Modify: `docs/bug-list.md`（追加一条已修复条目）

**Interfaces:**
- Consumes: Task 1–3 的产出
- Produces: 无

- [ ] **Step 1: 跑前端全量测试**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run
npx tsc --noEmit
```

Expected: 全绿；`tsc` 无输出。

- [ ] **Step 2: 跑 Rust 相关 crate**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/yi-agent-rs
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-llm
cargo test -p yi-agent-app-server --lib
```

Expected: 全绿（`translate.rs` 未改，`thinking_block_becomes_a_credential_free_thinking_item` 与 `thinking_item_round_trips_and_stays_credential_free` 必须仍通过）。

- [ ] **Step 3: 跑完整 CI 门禁**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display/yi-agent-rs
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt --all
just ci
```

Expected: 末尾输出 `CI passed`。首次跑需要全量编译，**耗时远超 2 分钟**（本次实测
`just ci` 在 120 秒上限内无法完成全量编译）。建议分两步：

1. 先用宽裕超时跑一次预热：`cargo build --workspace --all-targets`（首次十几分钟，属正常）。
2. 再跑 `just ci`，此时绝大多数编译已命中缓存。

若 `desktop-check` 步骤失败，先确认 Step 1 是绿的，再检查失败是否与本改动无关（记录
输出，不要盲目改代码）。

**已实测通过的等价门禁**（2026-11-02，本次在 worktree 里逐项跑过，可作替代证据）：

```bash
# Rust
cargo fmt --all -- --check                       # FMT_OK
cargo clippy -p yi-agent-llm -p yi-agent-app-server --all-targets   # 仅既有 html5ever 版本警告
cargo test -p yi-agent-llm                      # 188 passed
cargo test -p yi-agent-app-server --lib         # 1159 passed
# 前端
cd ../desktop && npx tsc --noEmit && npx vitest run   # TSC_OK, 1970 passed
```

- [ ] **Step 4: 更新 `docs/project-management/desktop.md`**

把 `:130` 这一行：

```markdown
- [ ] reasoning / thinking 展示 — core `ProviderEvent`（`yi-agent-rs/crates/yi-agent-core/src/provider.rs:43`）与 `AgentEvent`（`yi-agent-rs/crates/yi-agent-core/src/agent.rs:198`）当前无 thinking 变体，provider 未解析 thinking block；需 core + provider + protocol + translate + 前端逐层加支持；判据：发一条触发 extended thinking 的 prompt，GUI 显示 reasoning 内容
```

替换为：

```markdown
- [x] reasoning / thinking 展示 — 三层已贯通：core `ProviderEvent::ThinkingBlock` / `AgentEvent::ThinkingBlock`（`yi-agent-rs/crates/yi-agent-core/src/provider.rs`、`agent.rs`）→ `translate.rs` 产出**不携带凭证**的 `Item::Thinking`（`yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:448`）→ 前端 `Session` 把 `thread/reasoningDelta` 合并成块、再由服务端块级条目就地接管（`desktop/src/lib/session.ts` 的 `mergeThinkingText`）→ `ChatView` 的 `ReasoningBlock` 三态渲染（`desktop/src/components/ChatView.tsx:61`）。Responses 通道的展示明文由 `openai/responses/stream.rs` 从上游 `item.summary[].text` 拼出填入 `ThinkingBlock.text`（修复前恒为空串）。判据：`cd yi-agent-rs && cargo test -p yi-agent-llm`（含 `reasoning_item_summary_text_is_carried_into_the_thinking_block`、`empty_summary_yields_an_empty_thinking_block_text`）+ `cd desktop && npx vitest run src/lib/session.test.ts src/components/ChatView.test.tsx` — [设计](../superpowers/specs/2026-11-02-gpt-cot-display-design.md)
```

- [ ] **Step 5: 更新计数**

`docs/project-management/desktop.md` 的 `[x]` 由 112 变 113、`[ ]` 由 11 变 10。执行前先核对：

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display
grep -c '^- \[x\]' docs/project-management/desktop.md
grep -c '^- \[ \]' docs/project-management/desktop.md
```

把 `docs/project-management/README.md:32` 的：

```markdown
| desktop | 111 / 122 | [详情](./desktop.md) |
```

改为实际新计数（形如 `| desktop | 112 / 122 | [详情](./desktop.md) |`——分母不变，分子加一；以上面 `grep` 的实际输出为准）。

- [ ] **Step 6: 追加 bug-list 条目**

在 `docs/bug-list.md` 末尾追加（保持文件既有的空行分隔风格）：

```markdown
- [x] 桌面 App 用 GPT 模型（Responses 通道）时，「思考」块点开是空白（根因两个，独立叠加：(1) **展示侧**——`openai/responses/stream.rs` 构造块级 `ThinkingBlock` 时把 `text` 写死为 `String::new()`，摘要只经 `response.reasoning_summary_text.delta` 走 `ReasoningDelta` 旁路；前端 `session.ts` 的「就地接管」用该条目整体替换本地块，把已流式收到的摘要清空，且配对的 `item/completed`（同样空文本）会**再清一次**，`upsertItems` 回放补帧亦然；(2) **请求侧**——`openai/responses/types.rs` 在 `reasoning_effort` 为 `None` 时整个 `reasoning` 块都不发，`summary:"auto"` 随之消失，而用户清单里 Sol/Luna 的该字段为 `null`。实测直连 horizon 网关：不带 `reasoning` → `summary: []` + `encrypted_content` 约 5010 字节、零摘要增量；带 `effort:"high"` → 79~82 条摘要增量；`item.done` 的 `summary[]` 拼接与流式 delta **逐字相等**）。修复（本期只修展示侧，请求侧按用户选择保持原样）：`responses/stream.rs` 从 `item.summary[].text` 按序拼出展示文本填入 `ThinkingBlock.text`（凭证里的 `summary` 保持上游原文）；`desktop/src/lib/session.ts` 新增 `mergeThinkingText`，接管处 / 配对 `completed` / `upsertItems` 三处统一「空文本沿用旧值、`redacted` 一律不保留明文」；`ChatView` 的 `ReasoningBlock` 三态渲染（已加密 / 原文 / **已推理（无明文）**）。验证：`cargo test -p yi-agent-llm`（含 `reasoning_item_summary_text_is_carried_into_the_thinking_block`、`empty_summary_yields_an_empty_thinking_block_text`，以及既有的 `reasoning_item_done_captures_summary_verbatim` 仍逐字保真）、`cargo test -p yi-agent-app-server --lib`、`cd desktop && npx vitest run`（`session.test.ts` 80 → 85，`ChatView.test.tsx` 新增两例）、`npx tsc --noEmit`、`just ci`。**判别性证据**：`item/started` 与 `item/completed` 的空文本清空路径已用临时 vitest 用例在未修复的 `session.ts` 上实测复现（前者清空、后者在文本被补回后再次清空）—— [设计](superpowers/specs/2026-11-02-gpt-cot-display-design.md)
```

- [ ] **Step 7: 提交文档**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/gpt-cot-display
git add docs/project-management/desktop.md docs/project-management/README.md docs/bug-list.md
git commit -m "docs: 同步 GPT 思考块修复的模块状态与 bug 记录"
```

- [ ] **Step 8: 人工验收（不可自动化）**

在 App 里分别验证两种配置：

1. 给 `HORIZON-OG-Sol` 配 `reasoning_effort: high` → 提一个需要推理的问题（例如「17 只羊，除了 9 只都跑了，又买了 3 只，卖出一半，剩几只？」），展开「思考」应看到摘要文本。
2. 用不配 effort 的条目 → 同一问题，展开「思考」应显示「已推理（无明文）」，**不是**空白。

Expected: 两种都不出现空白卡片。若第 1 种仍空白，先确认模型条目确实保存了 effort（`~/.yi-agent/models.json` 里该条目的 `reasoning_effort` 非 null 且 probe 支持该档），再用 §Step 1 的测试确认后端改动已生效。

---

## 附录：验收汇总

| 判据 | 命令 | 期望 |
|---|---|---|
| 后端拼出展示文本 | `cargo test -p yi-agent-llm --lib reasoning_item_summary_text_is_carried_into_the_thinking_block` | PASS |
| 空摘要降级 | `cargo test -p yi-agent-llm --lib empty_summary_yields_an_empty_thinking_block_text` | PASS |
| 凭证逐字保真 | `cargo test -p yi-agent-llm --lib reasoning_item_done_captures_summary_verbatim` | PASS |
| 前端守卫 | `npx vitest run src/lib/session.test.ts` | 85 passed |
| 三态渲染 | `npx vitest run src/components/ChatView.test.tsx` | 全绿 |
| 类型 | `npx tsc --noEmit` | 无输出 |
| 全门禁 | `just ci`（在 `yi-agent-rs/`） | `CI passed` |
