# 设计：首轮结束后调用一次 LLM 生成 thread 标题

日期：2026-10-09
状态：待评审

## 1. 背景与问题

thread 的标题（侧栏每个会话显示的小标题）目前由首轮 prompt 直接截断而来：

- `ThreadStore::touch`（`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`）
  在每 turn 结束时调用；当 `meta.title` 仍为 `None` 时，用本轮 prompt 经
  `title_from` 处理（`split_whitespace` 归一化后取前 30 个字符）写入。
- 因此标题就是"用户第一句话的前 30 个字"。当用户第一句话说"帮我看看这个"、
  "再改一下"之类指代性强的短语时，标题毫无信息量，用户无法在侧栏区分多个会话。

用户要求：thread 的名字可以考虑调用一次 LLM 去生成对应的小标题。

## 2. 目标与非目标

### 目标

1. thread **首轮**对话结束后，用当前模型的模型发起**一次**短 LLM 调用，基于
   首轮材料产出贴切的小标题，写入 `meta.title`。
2. 该调用失败、超时或产出无法用（空/异常）时，**静默退回**现有的截断标题，
   绝不打断 turn、绝不让标题为空。

### 非目标

- 不新增独立的"标题模型"配置项；复用会话当前正在用的模型（`config.model`）。
- 不改 `thread/rename` RPC 与桌面端 UI；不改 `thread/list(All)` 的返回格式。
- 不在后续轮次重新生成标题，不做"内容变了就更新标题"。
- 用户手动改名后不再被自动生成覆盖。
- 不做重试/退避/多候选挑选。

## 3. 术语

- **首轮**：某 thread 的第一个 turn 收尾时，`meta.title` 尚为 `None` 的那一次。
- **兜底标题（fallback）**：`touch` 用 `title_from(prompt)` 写入的截断标题。
- **材料（material）**：喂给标题模型的文本，由首轮 user prompt 与首轮 assistant
  回复拼接而成。
- **CAS 写入**：仅当 `meta.title` 仍等于我们写入的兜底值时才替换，用于让手动
  改名优先于自动生成。

## 4. 设计

### 4.1 触发点与数据流

触发点固定在 `server.rs` 的 `persist_and_finish_turn`：在 `append_turn` 成功、
调用 `touch` 之后。伪代码：

```
append_turn(thread_id, turn)
  → let wrote = store.touch(thread_id, Some(prompt))   // 返回 bool：本轮是否首次写入标题
  → if wrote:                                          // 仅首轮
        first_assistant = 首轮第一条 AgentMessage 的 text       // 取不到则 None
        title = timeout(TITLE_TIMEOUT,
                        generate_title(provider, config, user_prompt, first_assistant))
                     .ok()          // 超时 → None
                     .and_then(|r| r.ok())   // provider 错误 → None
                     .flatten()     // 空标题 → None
        if let Some(t) = title:
            store.set_title_if_unchanged(thread_id, &title_from(user_prompt), &t)
```

- `touch` 返回值承担"是否为首轮"的判定，无需额外读盘。
- CAS 的 `expected` 即 `title_from(user_prompt)`（与 `touch` 内部写入的值一致）。
- 首轮 assistant 文本取 `completed_items` 中第一条 `Item::AgentMessage { text }`；
  取不到（例如首轮纯工具调用无文本）时，材料只含 user prompt。

### 4.2 组件划分

#### A. `yi-agent-core` 新增 `title.rs`（对齐 `compact.rs` 的风格）

- `const TITLE_INSTRUCTIONS: &str` —— 系统提示。要求：输出**一个**简短标题，
  语言跟随用户提问（中文提问给中文标题，英文给英文），不超过约 20 个汉字 /
  40 字符，不含换行、引号、"标题："前缀或任何解释。
- `pub async fn generate_title(provider: &Arc<dyn Provider>, config: &AgentConfig,
  user_text: &str, assistant_text: Option<&str>) -> Result<Option<String>, AgentError>`
  - 构造 `ProviderRequest`：`model = config.model`；`system = Some(TITLE_INSTRUCTIONS)`；
    `messages = vec![Message::user(build_title_material(user_text, assistant_text))]`；
    `tools = vec![]`；`params = config.gen_params.clone()` 但 `max_tokens = Some(64)`
    （限制成本；标题极短）。
  - 累加响应文本 → `sanitize_title`。空结果返回 `Ok(None)`；provider 错误上抛
    `Err(AgentError::Provider)`。
- `fn build_title_material(user_text: &str, assistant_text: Option<&str>) -> String`
  —— 纯函数，可测。两侧各自截断到 2000 字符后按带标签的结构拼接（如
  `用户提问：…\n\n助手回复：…`），`assistant_text` 为 `None` 时省略该段。
- `fn sanitize_title(raw: &str) -> Option<String>`
  —— 纯函数，可测。步骤：去首尾空白 → 取首个非空行 → 去除包裹引号
  （`"` `'` `「」` `“”`）→ 去除 `标题:` / `Title:` 前缀 → 截断至 30 字符
  （与现状 `title_from` 的上限一致）→ 结果为空则 `None`。
- `lib.rs` 追加导出 `title::{generate_title, TITLE_INSTRUCTIONS}`。

#### B. `yi-agent-app-server/src/thread_store.rs`

- `touch` 签名由 `io::Result<()>` 改为 `io::Result<bool>`：`true` = 本次调用了
  `title_from` 并写入标题（即首轮）；`false` = 已有标题或未写入。未知 id 仍静默
  `Ok(false)`。写入逻辑本身不变（仍用 `title_from(prompt)` 兜底）。
- 新增 `pub fn set_title_if_unchanged(&self, id: &str, expected: &str,
  new: &str) -> io::Result<bool>`：走同一把 `update_meta` 读-改-写锁；仅当
  `meta.title.as_deref() == Some(expected)` 时替换为 `new` 并 bump `updated_at`，
  否则返回 `Ok(false)`。id 非法返回 `Err`，未知 id 返回 `Ok(false)`。
- `title_from` 由私有提升为 `pub(crate)`，供 `server.rs` 计算 `fallback`。

#### C. `yi-agent-app-server/src/server.rs`

- `persist_and_finish_turn` 增加两个参数：`provider: &Arc<dyn Provider>`、
  `config: &AgentConfig`。两处调用点（常规收尾 `Some(&turn_id)` 路径与 session
  命令路径）作用域内均已有这些值。
- 在 `touch` 之后按 4.1 实现"首轮则生成 → CAS 写入"。
- 常量 `TITLE_TIMEOUT: Duration = Duration::from_secs(5)`。
- 仅当 `user_prompt` 为 `Some`（真实 turn，非 clear/compact 收尾）才尝试生成，
  与 `touch` 的触发条件一致。

### 4.3 错误处理

- 生成过程任何失败（provider 报错、超时、空标题、sanitize 后为空）都**不报错、
  不打断 turn**：标题保持 `touch` 写入的兜底值，仅记一行 stderr / tracing。
- `set_title_if_unchanged` 因并发改名而跳过：静默成功（用户意图优先）。
- 仅首轮触发，后续轮次零额外开销。

### 4.4 测试

- core `title.rs`：
  - `sanitize_title` 覆盖引号包裹、`标题:` 前缀、多行取首行、超长截断、CJK 长度、
    全空白 → `None`。
  - `build_title_material` 覆盖两侧截断、单侧为空。
  - `generate_title` 用 mock provider 覆盖：正常文本 → 清洗后标题；空文本 →
    `Ok(None)`；provider 返回错误 → `Err`。
- app-server `thread_store`：
  - `touch` 返回值：首轮 `true`、已有标题 `false`、未知 id `false`。
  - `set_title_if_unchanged`：命中替换 + bump、被并发改名拦截返回 `false`。
- app-server 集成：
  - 首轮 turn 用返回固定文本的 mock provider，断言最终 meta 标题为 LLM 标题
    （而非截断标题）。
  - 用会失败的 provider（测试中已有），断言标题回退为截断标题且 turn 正常收尾。

## 5. 影响面

- 行为变化仅限"首轮标题来源"。首轮收尾增加一次短路 LLM 调用，最长 5s 超时，
  通常数百毫秒；超时/失败回退，不影响 turn 完成。
- 无协议、无前端改动；`thread/list(All)` 读的就是 `meta.title`。
- **标题不会随 `turn/completed` 自动刷新（已知限制）。** `turn/completed` 由
  translator 在 `persist_and_finish_turn` **之前**发出，而兜底标题与随后可能的
  LLM 标题都写在 `persist_and_finish_turn` **之后**。因此桌面端在本轮 `turn/completed`
  触发的 `refreshThreads()` 只能拿到兜底截断标题；LLM 标题要等到下一次**无关的**
  刷新（后续轮的 `thread/listAll`、resume、或重连）才会显示。这是本分支**有意**
  接受的行为，见下。
- 手动改名（`thread/rename`）优先级通过 CAS 得以保留。

### 5.1 已知限制 / 后续

- **本轮不修**：让 LLM 标题在侧栏即时可见，需要在标题落盘后主动通知桌面端——即
  新增一条 thread 元数据变更通知（`thread/titleChanged` 之类）。本分支明确约定
  "不改协议、不碰桌面端"，故**不**添加任何协议消息或通知，仅在此记录该限制。
- 后续若要做：在 `set_title_if_unchanged` 成功后广播一条独立的元数据变更通知，
  由桌面端据此仅刷新该 thread 的标题；届时合入应同步更新本节与相关测试。
