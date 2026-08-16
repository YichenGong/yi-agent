# 权限管理设计

日期:2026-07-25
状态:设计已验证,待实现

## 概述

为 yi-agent 加上权限管理功能,参考 codex 的 `--yolo` 和 claude 的 `--dangerously-skip-permissions`。

非 yolo 模式下,需要授权的工具调用(bash、write、edit)会向用户请求确认。用户可通过白名单逐渐放开权限,减少重复确认。黑名单命令作为最后防线,任何模式下都需用户主动确认才能执行。

## 设计决策汇总

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 需要授权的工具范围 | bash + write + edit | 和现有 `requires_confirmation` 标记一致 |
| 白名单粒度 | 分层:工具类型 + 命令前缀/路径模式 | 兼顾简单和精细,粗细可选 |
| 白名单持久化 | 项目级 `.yi-agent/permissions.toml`,无全局 | 跟项目走,不跨项目污染 |
| 黑名单行为 | 默认拒绝,用户可主动确认执行 | 给用户最终决定权,但默认保守 |
| 确认 UI | 事件通道 + 渲染层各自呈现 | 和现有 `Renderer` trait 架构一致 |
| 确认选项 | 允许本次 / 始终允许此类工具 / 始终允许此命令前缀 / 拒绝 | 对应分层白名单 |
| 前缀提取 | LLM 提取,匹配用确定字符串比对 | 确定性匹配 + 灵活提取 |
| LLM 不可用降级 | 仅"允许本次",不写白名单 | 保守,不错误放大权限 |
| 白名单文件格式 | 按层级分组(bash 用前缀,write/edit 用路径) | 语义清晰 |
| 路径匹配 | glob 通配符 | 用户熟悉 `.gitignore` 式语法 |
| LLM 超时 | 15 秒 | 5 秒太短 |

## 1. 总体架构和检查流程

权限检查发生在 agent loop 调用 `tool.call()` 之前,在 `agent.rs` 的 ACT 循环里。每次工具调用前,过一遍权限检查器 `PermissionChecker`。

### 检查顺序(从宽到严)

1. **工具类型层**:如果该工具在 `permissions.toml` 的 `[tool-level]` 里标记为 `true`(如 `bash = true`),直接放行,跳过后续检查。
2. **前缀/路径层**:
   - bash:用前缀字符串匹配 `[prefix-level.bash].prefixes`,命中则放行。
   - write/edit:用 glob 匹配 `[prefix-level.write].paths`,命中目标路径则放行。
3. **黑名单**:对 bash 命令过现有 `blocklist.rs`,命中则走黑名单确认流程。
4. **未命中任何白名单**:走正常确认流程,弹四个选项。

### yolo 模式行为

- 跳过步骤 1-2 的白名单检查(视为全部命中工具类型层)
- 步骤 3 黑名单**不跳过**,仍需确认
- 步骤 4 不会发生

黑名单是 yolo 也拦的最后一道防线。

## 2. PermissionChecker 数据结构和配置加载

`PermissionChecker` 放在 `yi-agent-core` crate,被 agent loop 使用,不依赖渲染层。

```rust
// crates/yi-agent-core/src/permission.rs

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub tool_level: ToolLevelConfig,
    #[serde(default)]
    pub prefix_level: PrefixLevelConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ToolLevelConfig {
    pub bash: bool,
    pub write: bool,
    pub edit: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PrefixLevelConfig {
    pub bash: BashPrefixConfig,
    pub write: PathPrefixConfig,
    pub edit: PathPrefixConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BashPrefixConfig {
    pub prefixes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PathPrefixConfig {
    pub paths: Vec<String>,  // glob 模式
}
```

### 加载时机

- agent 启动时从 `<workdir>/.yi-agent/permissions.toml` 读取
- 文件不存在时用 `Default::default()`(全 false,空列表 — 所有工具都要确认)
- 仅项目级,不读全局

### 运行时写入

- 用户选"始终允许此类工具"或"始终允许此命令前缀"时,更新内存中的 `PermissionsConfig`,并写回 `.yi-agent/permissions.toml`
- 写回逻辑在 `PermissionChecker` 里,用 `tokio::fs::write` + `toml::to_string`
- 文件不存在时自动创建 `.yi-agent/` 目录

### yolo 标志传递

`Config` 加 `yolo: bool` 字段,从 CLI `--yolo` 读取。传给 `PermissionChecker::new(config, yolo)`。

## 3. 确认事件流和用户决策通道

agent loop 需要向渲染层请求用户决策,并阻塞等待。现有架构里 agent 和渲染层通过事件通道单向通信(agent → 渲染层),需要扩展一条回传通道。

### 新增事件类型

```rust
pub enum AgentEvent {
    // ... 现有事件 ...
    PermissionRequest {
        request_id: u64,
        tool_name: String,
        tool_input: Value,
        prefix_suggestion: Option<String>,  // LLM 提取的前缀,None 表示 LLM 不可用
        kind: PermissionKind,
    },
    PermissionResolved {
        request_id: u64,
        decision: Decision,
    },
}

pub enum Decision {
    AllowOnce,
    AlwaysAllowTool,
    AlwaysAllowPrefix(String),  // 用户确认的前缀
    Deny,
}

pub enum PermissionKind {
    Normal,
    Blacklisted(String),  // blocklist 命中的 reason
}
```

### 通道扩展

- agent loop 持有 `decision_tx` 的对端,渲染层通过它回传决策
- `PermissionRequest` 事件里带 `request_id`,渲染层把决策通过 `decision_tx` 发回
- agent loop 在 `decision_rx` 上等对应 `request_id` 的响应

### agent loop 等待逻辑

```rust
// 在 tool.call() 之前
let decision = permission_checker.check(&tool_name, &tool_input);
match decision {
    CheckResult::Allow => tool.call(input).await,
    CheckResult::Deny => ToolResult::error("denied by permission"),
    CheckResult::NeedConfirm(req) => {
        event_tx.send(AgentEvent::PermissionRequest(req.clone())).await?;
        let decision = wait_for_decision(decision_rx, req.request_id).await?;
        permission_checker.apply_decision(&tool_name, &tool_input, &decision).await?;
        match decision {
            Decision::AllowOnce | Decision::AlwaysAllowTool | Decision::AlwaysAllowPrefix(_) => tool.call(input).await,
            Decision::Deny => ToolResult::error("user denied"),
        }
    }
    CheckResult::Blacklisted(req) => {
        // 同 NeedConfirm,但 kind=Blacklisted,UI 默认高亮"拒绝"
    }
}
```

`request_id` 用 `AtomicU64` 递增,保证唯一。

## 4. LLM 前缀提取

LLM 提取前缀的逻辑封装为 `prefix_extractor` 模块,放在 `yi-agent-core`。

### 调用时机

- 只对 `bash` 工具调用 LLM 提取前缀,write/edit 不需要(它们的"前缀"是路径,直接从 `tool_input` 里取 `path` 字段)
- 在 `PermissionChecker::check()` 判断需要确认时,先调 LLM 提取前缀,把结果放进 `PermissionRequest.prefix_suggestion`

### LLM 调用

- 复用现有 `yi-agent-llm` 的 provider 抽象,用非流式调用
- 用和 agent 主循环相同的 model(从 `Config` 读)
- prompt 大意:`"从以下 shell 命令提取命令前缀(命令名 + 子命令,不含参数)。只返回前缀字符串,不要其他内容。命令: {command}"`

### 超时

15 秒超时,超时视为 LLM 不可用,`prefix_suggestion = None`,降级到"仅本次允许"。

### 边界情况

- 命令为空或极短(如 `ls`):不调 LLM,用命令本身作前缀
- 命令含管道、重定向(`git status | grep foo`):提取第一个管道前的命令前缀(`git status`)

## 5. TUI 模式的确认呈现

TUI 模式用专用 `HistoryCell::PermissionRequest` cell 呈现确认提示。

### 新增 cell 类型

```rust
pub enum HistoryCell {
    // ... 现有 ...
    PermissionRequest {
        request_id: u64,
        tool_name: String,
        display: String,
        prefix_suggestion: Option<String>,
        kind: PermissionKind,
    },
    PermissionResolved {
        request_id: u64,
        decision: Decision,
    },
}
```

### 渲染

```
┌─ Permission Required ─────────────────────┐
│ bash: git push origin main                │
│ (blacklisted: git push may rewrite history)│
├───────────────────────────────────────────┤
│ [1] Allow Once                             │
│ [2] Always Allow This Tool (bash)          │
│ [3] Always Allow Prefix: "git push"        │
│ [4] Deny                                  │
└───────────────────────────────────────────┘
```

- 黑名单场景下,"Deny" 高亮为默认选中
- 普通场景下,"Allow Once" 高亮为默认
- LLM 不可用时,`[3]` 行不显示

### 按键处理

- 数字键 `1`-`4` 选择对应选项
- `Enter` 确认默认选项
- 黑名单场景下选 "Always Allow" 时显示警告,但仍允许

### 前缀可编辑

选"3"后可弹出输入框让用户编辑前缀。第一版可先用 LLM 提取的前缀直接写入,用户不满意下次再改。前缀可编辑放到后续迭代。

## 6. Inline 模式的确认呈现

Inline 模式用 reedline 的 `external_printer` 流式输出文本,确认流程用 stdin 读取。

### 输出格式

```
━━━ Permission Required ━━━━━━━━━━━━━━━━━━━━━━━
bash: git push origin main
(blacklisted: git push may rewrite history)
[1] Allow Once
[2] Always Allow This Tool (bash)
[3] Always Allow Prefix: "git push"
[4] Deny
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Choice (1-4) [default: 1]:
```

### 实现

- `InlineRenderer` 实现 `Renderer` trait 的新方法 `render_permission_request`
- 打印上述文本后,阻塞从 stdin 读一行用户输入
- 确认期间暂停 reedline 的行编辑,用 `tokio::io::stdin().read_line()` 直接读一行,读完恢复

### 超时

不给确认操作加超时,用户可以慢慢想。agent loop 阻塞在 `decision_rx` 上,不消耗资源。

## 7. CLI 标志和 Config 集成

### CLI 标志

```rust
#[derive(Parser)]
struct Cli {
    // ... 现有 ...
    #[arg(long, help = "Skip permission prompts (except blacklisted commands)")]
    yolo: bool,
    #[arg(long = "dangerously-skip-permissions", alias = "yolo", help = "Alias for --yolo")]
    skip_permissions: bool,
}
```

两个标志等价,任一为 true 即 yolo 模式。

### Config 集成

```rust
pub struct Config {
    // ... 现有 ...
    pub yolo: bool,
}
```

加载优先级:CLI `--yolo` / `--dangerously-skip-permissions` > env var `YI_AGENT_YOLO` > `false`。

### 环境变量

- `YI_AGENT_YOLO=true` 也启用 yolo,方便 `.env` 里设默认
- `.yi-agent/.env` 设了项目级默认 yolo,用户可 CLI 覆盖

yolo 是 Config 层属性,和渲染模式无关。

## 8. 黑名单集成和 write/edit 路径提取

### 黑名单集成

- 现有 `crates/yi-agent-tools/src/shell/blocklist.rs` 保持不变
- `PermissionChecker::check()` 对 bash 命令的检查顺序:工具类型层 → 前缀层 → 黑名单
- **白名单命中也过黑名单**,黑名单是绝对最后防线。yolo 和非 yolo 行为一致,黑名单始终生效
- 黑名单命中后,用户仍可主动选"Allow Once"或"Always Allow Tool"执行(对应设计决策:默认拒绝但可手动确认)

### write/edit 路径提取

- write/edit 工具的 `tool_input` 里有 `path` 或 `file_path` 字段
- `PermissionChecker` 从 `tool_input` 提取路径,用 `glob` crate 匹配 `paths` 列表
- 路径用绝对路径比对(相对 workdir 解析后)

## 9. 单元测试要求

`PermissionChecker` 是权限核心,测试要充分覆盖所有分支。

### 测试覆盖矩阵

| 场景 | 白名单层 | 黑名单 | yolo | 预期 |
|------|----------|--------|------|------|
| 工具类型层放行 | `bash=true` | 不命中 | 任意 | Allow |
| 前缀层放行 | `prefixes=["git push"]` | 不命中 | 任意 | Allow |
| 白名单+黑名单命中 | `bash=true` | 命中 | 任意 | Blacklisted |
| 全无白名单 | 空 | 不命中 | false | NeedConfirm |
| 全无白名单 yolo | 空 | 不命中 | true | Allow |
| 全无白名单+黑名单 yolo | 空 | 命中 | true | Blacklisted |
| LLM 提取前缀 | 空 | 不命中 | false | NeedConfirm + prefix_suggestion=Some |
| LLM 超时 | 空 | 不命中 | false | NeedConfirm + prefix_suggestion=None |

### 具体测试用例

1. `check` 函数所有分支
2. `apply_decision` 各 Decision 变体更新 `PermissionsConfig` 的正确性
3. `PermissionsConfig` 序列化/反序列化往返
4. glob 路径匹配:`src/**` 命中 `src/foo.rs`、不命中 `tests/foo.rs`
5. 前缀匹配:`git push` 命中 `git push origin main`、不命中 `git status`
6. 管道命令前缀提取:`git status | grep foo` → `git status`
7. LLM 不可用降级:mock LLM 返回错误,验证 `prefix_suggestion=None`
8. 决策通道:发 `PermissionRequest`,模拟渲染层回 `Decision`,验证 agent loop 继续
9. 黑名单命令命中后 `apply_decision(AlwaysAllowTool)` 仍能执行
10. `permissions.toml` 读写往返:写入后重新加载,字段一致

### 测试基础设施

- LLM mock:`yi-agent-llm` 里加 mock provider,或用 trait 隔离在 `PermissionChecker` 里
- 临时文件:用 `tempfile` crate 创建临时 `.yi-agent/` 目录做集成测试
- 决策通道:用 `tokio::sync::mpsc` channel 直接构造,无需渲染层

## 10. 黑名单单元测试扩充

现有 `blocklist.rs` 里的正则模式都要覆盖,每个模式至少多个用例(正例 + 反例 + 边界)。

### 测试用例枚举

1. **`rm -rf /` 类**:
   - 正例:`rm -rf /`、`rm -rf /*`、`rm -rf ~`、`rm -rf $HOME`、`rm -rf *`、`rm -rf ./`、`rm -fr /`、`rm -r -f /`
   - 反例:`rm -rf build/`、`rm -rf ./target`、`rm foo.txt`、`rm -rf src/`、`cargo rm`
   - 边界:`rm -rf / `(尾空格)、`sudo rm -rf /`、`rm -rf --no-preserve-root /`

2. **fork bomb**:
   - 正例:`:(){ :|:& };:`、各种空格变体、`bash -c ':(){ :|:& };:'`
   - 反例:`echo ":(){ :|:& };:"`、`cat file.sh`(内容含 fork bomb 但只是显示)
   - 边界:`: () { : | & } ; :`(带空格)、`fork() { fork|fork& }; fork`

3. **`npm publish`**:
   - 正例:`npm publish`、`npm publish --access public`、`npm publish .`
   - 反例:`npm install`、`npm run build`、`npm unpublish`、`echo npm publish`
   - 边界:`npm publish --tag beta`、`npm publish --dry-run`

4. **其他现有 blocklist 模式**:逐个覆盖,每个 3-5 个用例

5. **组合命令**:
   - `git status && rm -rf /`:含黑名单子命令,应该拦截
   - `rm -rf / || echo done`:应该拦截
   - `echo "rm -rf /"`:引号内,不应拦截

6. **绕过尝试**:
   - `r""m -rf /`(引号干扰):确认现有正则能否拦截
   - `rm -rf  /`(多空格):应该拦截
   - `rm -rf /tmp/../`(`..` 跳转):应该拦截

### 测试生成方式

用 `rstest` crate 的参数化测试,更清晰:

```rust
#[rstest]
#[case("rm -rf /", true)]
#[case("rm -rf build/", false)]
#[case("sudo rm -rf /", true)]
fn test_rm_rf(#[case] cmd: &str, #[case] blocked: bool) {
    assert_eq!(blocklist(cmd).is_some(), blocked);
}
```

新增测试依赖:`rstest` 加到 `yi-agent-tools` 的 `[dev-dependencies]`。

## 待确认问题

实现时需确认:

1. write/edit 工具的 `tool_input` 里路径字段名(`path` 还是 `file_path`)
2. `npm publish --dry-run` 是否拦截(dry-run 不实际发布,可能不需要拦截)
3. `r""m -rf /` 这类引号绕过,现有正则是否覆盖,如不覆盖是否需要在实现时加强

## 实现顺序建议

1. `PermissionsConfig` 数据结构 + 序列化/反序列化 + 单元测试
2. `PermissionChecker::check()` 核心逻辑 + 单元测试
3. `apply_decision` 逻辑 + 单元测试
4. 黑名单单元测试扩充(枚举所有现有模式)
5. LLM 前缀提取模块 + mock 测试
6. 事件类型扩展 + 决策通道
7. agent loop 集成 `PermissionChecker`
8. CLI `--yolo` / `--dangerously-skip-permissions` 标志 + Config 集成
9. TUI 确认呈现
10. Inline 确认呈现
11. 集成测试(agent loop + 渲染层 mock)
