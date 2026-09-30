# Desktop：工具卡片摘要 + 进度叙述提示词 设计

- 日期：2026-09-30
- 状态：已确认，待实现
- 相关：`docs/bug-list.md`（本设计同时解决"bash 卡片只看得到 `bash`"与"工具块之间没有文字讲解"两条体验反馈）

## 1. 背景

桌面端跑长任务时，用户看到的是大量只有 `bash` 字样的工具卡片，块与块之间没有任何文字说明。实测证据（线上同一 model/provider/系统提示词）：

- `.yi-agent/threads/thread-dfb16d54-*.jsonl`：63 个 `toolCall` item 对 3 个 `agentMessage`，最长连续无文字工具卡 47 个。
- 全部 trace 里 `content_blocks <= tool_count`（该轮没有任何文本块）的 tool 往返占 55.3%；只看每个 turn 的第 1 步为 59.4%；仅 app-server 会话为 69%（774/1124）。

## 2. 根因

两个独立问题：

1. **模型不写过渡文字**：`default_system_prompt()`（`yi-agent-rs/crates/yi-agent-core/src/agent.rs`）只约束了 emoji 与"最小化往返 / 合并 bash"，**没有任何叙述要求**；紧邻的 `minimizing round-trips` 与 bash 工具 description 的 `Prefer combining dependent steps with &&` 反而把模型推向"一句话不说、直接并发多个 tool call"。
2. **卡片头部只有工具名**：`desktop/src/components/ToolCallCard.tsx` 的头部只渲染 `item.name`；`item.input` 完整可用（`protocol.rs` `Item::ToolCall`），但只放在默认折叠的 Input 区里。TUI 早已是 `name(input)`（`tui/cell.rs` `render_tool_call`）+ 语义化摘要（`tui/history.rs` `permission_summary`），桌面端从未接上。

放大因素（本次不处理）：`reasoning_content` 被 provider 层丢弃（`llm/src/openai/stream.rs` 只读 `content`），模型"想了但没说"的过程完全不可见。

## 3. 目标与非目标

**目标**
- 模型在工具调用之间至少给出简短叙述，且**至少每约 10 次工具调用**汇报一次在做什么。
- 折叠态的工具卡片本身就能说明"这条在干什么"。

**非目标**
- 不做代码层的强制注入（用户明确选择纯提示词方案）。因此"每 10 次"是**软约束**：靠模型遵守，不保证。
- 不加协议字段（`Item::ToolCall` 不加 `summary`），不改 `translate.rs` / `protocol.rs`。
- 不做 reasoning / thinking 展示。
- 不改 `minimizing round-trips` 段落与 bash 工具 description。

## 4. 决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 每 10 次叙述的落地方式 | 提示词（C1） | 用户选定；改动面最小，不动 agent loop |
| 摘要生成位置 | 前端纯函数 | 数据已在 `item.input` 里，零协议改动、零往返 |
| 摘要口径 | 对齐 TUI `permission_summary` | 两个前端对"在做什么"的理解一致，避免各写一套 |
| 摘要取不到值 | 返回 `null`，只显示工具名 | 退化为现状，绝不把长 JSON 灌进标题 |
| 截断 | 80 列 + 省略号 | 单行可读；完整内容始终在展开区 |

## 5. 设计：C1 提示词叙述规则

在 `default_system_prompt()` 的 `Style:` 段之后、`Task execution:` 之前插入：

```
Progress narration:
- Say what you are doing while you do it: a response that issues tool calls
  opens with 1-2 sentences of prose saying what you are about to do and why.
- Never let a long run of tool calls go silent. At least every ~10 tool calls,
  stop and narrate where you are, what you found so far, and what is next;
  narrating more often is fine.
- Narration rides along with the tool call in the same response. It never means
  splitting one tool call into several.
- Narrate in the language the user writes in.
```

四条要点及其作用：

1. 默认每条带 tool call 的响应都先说 1-2 句（解决"大部分块之间没字"）。
2. 每约 10 次工具调用必须有一次汇报（兜底密度；措辞为 `At least every ~10`，允许更频繁）。
3. 显式声明叙述**不**意味着拆分调用——否则与紧邻的 `minimizing round-trips` / `combine them into ONE bash call` 冲突，模型可能靠拆分命令来腾出说话位置，反而更慢。
4. 跟随用户语言（否则中文用户可能拿到英文旁白）。

提示词对所有入口生效（TUI、`run`、app-server、子 agent）。子 agent 输出会被父 agent 读取，多一句话无害。

## 6. 设计：A 卡片摘要

新增 `desktop/src/lib/toolSummary.ts`：

```ts
export function toolCallSummary(name: string, input: unknown): string | null
```

按工具名取一个字段，取到后压掉换行与多余空白，超过 80 列则截断加省略号；取不到、input 不是对象、字段缺失或非字符串一律返回 `null`。

| 工具 | 字段 |
|---|---|
| `bash` / `process_start` | `command` |
| `read` / `write` / `edit` / `view_image` | `path` |
| `grep` / `glob` | `pattern` |
| `Skill` | `path` |
| `web_search` | `query` |
| `web_fetch` | `url` |
| 其它 | ——（`null`） |

`ToolCallCard.tsx` 头部改为工具名 + 暗色截断摘要（带 `title` 便于悬停看全），右侧状态 pill 的 `ml-auto` 不动；展开区内容不变。

## 7. 测试

Rust（先在 worktree 跑基线）：
- 新增 `agent::tests::default_system_prompt_requires_progress_narration`，钉住四条要点的关键短语。
- 现有 `default_system_prompt_contains_identity_and_strategy` 必须继续通过，保证"最小化往返"未被误删。

前端：
- 新增 `desktop/src/lib/toolSummary.test.ts`（node 环境纯函数）：上表逐个覆盖，外加缺失字段 / 类型错误 / 超长 / 多行换行 / 非对象 input 的边界。
- 新增 `desktop/src/components/ToolCallCard.test.tsx`（jsdom）：折叠态标题含命令文本；未知工具的 input 不崩且只显示工具名；展开后仍能看到完整 Input JSON。

## 8. 取舍与已知缺口

- **C1 是软约束**：模型理论上可以不遵守。已实测同厂商同模型对叙述指令的服从度为 3/3 次运行零静默（4-8 步小任务），但长任务（本 bug 的 47 连击场景）无验证手段。若上线后仍静默，再考虑代码层兜底（本轮用户已明确不选）。
- 摘要不是语义标题，只是"取该工具最有信息量的那个字段"。真正语义化的标题（例如把 `git status && ls` 概括成"查看工作区状态"）需要 LLM 生成，不在本轮范围。

## 9. 验证清单

- `cargo test -p yi-agent-core --lib`
- `cargo test -p yi-agent --bin yi-agent`
- `cd desktop && npx vitest run && npx tsc --noEmit`
- `cargo fmt --all`（提交前）
- 同步更新 `docs/project-management/yi-agent-core.md`、`docs/project-management/desktop.md`、`docs/bug-list.md`。
