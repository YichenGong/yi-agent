# 模型配置门禁设计（首次安装必须显式确认）

## 状态

设计草案，等待用户 review。

## 背景与问题

首次安装跳过模型设置时，系统会静默填入一组默认 provider / url / model，并在整条链路上把它们当成"用户已确认"的配置使用。用户确认的是 **C 逻辑**：向导必填、必须测试通过才允许保存、运行期不再猜模型；OpenAI provider 的 URL 结尾缺少 `/v1` 时要提醒。

现状证据：

1. **向导静默回填默认值**：`desktop/src/components/OnboardingWizard.tsx:22-29` 定义 `DEFAULT_URL` / `DEFAULT_MODEL`，`:52-56` 的 `input()` 在模型或 URL 为空时回填（anthropic → `claude-sonnet-4-20250514`，openai → `gpt-4o`）。空着也能保存成功。
2. **测试失败仍可保存**：`OnboardingWizard.tsx:200-213` 在 `testReason !== null` 时把按钮文案改成「仍然保存」，点击照样落盘。
3. **运行期硬编码兜底**：`yi-agent-rs/crates/yi-agent-runtime/src/config.rs:286-293` 在无注入时给出默认 provider / url / model。清单为空时整条链路仍拿到 `claude-sonnet-4-20250514` 或 `gpt-4o`。
4. **子 agent 会拿到未确认的默认名**：`crates/yi-agent-subagent/src/attach.rs:114-130` 用 `resolve_effective(cfg, catalog, catalog.subagent_model)`；清单无条目时回落到 `cfg`，即第 3 条的默认名。`crates/yi-agent-subagent/src/lib.rs:70-100` 的刷新路径同源。
5. **设置页新增模型不会自动成为默认**：`crates/yi-agent-app-server/src/model_rpc.rs:202-300` 的 `upsert` 只 push 条目，不设 `default_model`；只有 `onboarding::import_settings_into_catalog`（`crates/yi-agent-runtime/src/onboarding.rs:186-227`）和 `model/importEnv`（`model_rpc.rs:354-398`）会设置默认。
6. **CLI 有同样的问题**：`crates/yi-agent/src/onboarding_prompt.rs:48-120` 用 `read_line_or` 回填默认值，连接失败后默认答案是「仍然保存」。

已有可复用能力：

- `onboarding::assess`（`crates/yi-agent-runtime/src/onboarding.rs:62-80`）已能在清单无可用默认条目时返回 `Needed { reasons }`。
- `validate_settings`（`onboarding.rs:115-140`）已拒绝空 model / 空 api_key，`api_url` 允许为空（语义 = 用 provider 默认地址）。
- `test_connection`（`onboarding.rs:272-318`）用临时 provider 发一条 `max_tokens=1` 的最小请求，15s 超时，不落盘。
- `onboarding/test` RPC（`crates/yi-agent-app-server/src/rpc/onboarding.rs:71-105`）需要 `Control` scope。
- `suspectsMissingV1`（`desktop/src/components/SettingsModelsTab.tsx:65-70`）已有 `/v1` 启发式。
- 错误族约定（`crates/yi-agent-app-server/src/protocol.rs:178-213`）：数值码是粗粒度兜底，`data.code` 是 UI 分支用的稳定词汇。

## 目标

1. 模型配置只能由用户显式确认：向导不再回填模型名，模型字段与密钥字段必填。
2. 保存（落盘）前必须有一次成功的连接测试；测试失败不允许写入，前端与 CLI 行为一致。
3. OpenAI 兼容接口的 URL 不以 `/v1` 结尾时，保存前给出明确提醒。
4. 运行期不再凭空生成模型名：没有显式注入、且清单没有可用默认条目时，开始对话返回可识别的「未配置模型」错误，而不是拿默认名发请求。
5. 设置页新增的第一个模型自动成为默认；默认条目缺失时界面能看见。

## 非目标

- 不改变清单（`~/.yi-agent/models.json`）的数据格式。
- 不改变 `subagent_model` → `default_model` 的解析优先级（`crates/yi-agent-runtime/src/models.rs:822-833`）。
- 不引入新的模型自动探测（如自动枚举 `/v1/models`）。
- 不改 TUI 的 `/model` 交互，只改首次引导。
- 不改变 `--model` / `--provider` / `--api-key` 整包注入的语义（`config.rs:267-276` 的 `model_injected`）。

## 设计

### 1. 向导：必填 + 测试门禁

`OnboardingWizard.tsx`：

- 删除 `DEFAULT_MODEL`，`DEFAULT_URL` 仅作为输入框 `placeholder`（仍是"建议值"，不是"已填值"）。`input()` 不再回填 model；`api_url` 留空时按空串提交，由后端按 provider 默认地址补齐（`import_settings_into_catalog` 现有行为，`onboarding.rs:193-203`）。
- 「保存」按钮在 `model.trim()` 或 `apiKey.trim()` 为空时禁用；输入框旁给出必填说明。
- 删除「仍然保存」分支。`testReason !== null` 时按钮保持禁用，只显示失败原因与「重新测试」。
- 连接测试状态显式三态：`untested` / `testing` / `passed` / `failed`。只有 `passed`（或后端 apply 内部测试通过）才允许完成。
- 「稍后设置」保留，但它只关闭向导，不产生配置。进入主界面后由空状态与常驻提示承担引导（见配套 spec）。

### 2. 后端：apply 自己验证，不信任前端

只靠前端禁用按钮不足以构成门禁（任何客户端都能直接调 RPC）。因此 `onboarding/apply` 在写入前必须自己跑一次连接测试：

- `crates/yi-agent-app-server/src/onboarding_rpc.rs` 的 `apply` 目前是同步函数；改为在 `rpc/onboarding.rs` 的 `"onboarding/apply"` 分支里先调用 `yi_agent_runtime::onboarding::test_connection(&settings).await`，通过后再调用 `handle_onboarding_request_at` 完成写入。
- 测试失败返回 `RpcError`，`data.code = "model_test_failed"`，`message` 带 `test_connection` 的中文原因，数值码沿用 `invalid_model` 族（`-32602`）。**零写入**。
- `onboarding/test` 保留为"预检"，让用户不必等 apply 才发现错误。两者都调用同一个 `test_connection`，结果口径一致。

CLI 对齐（`crates/yi-agent/src/onboarding_prompt.rs`）：

- 模型标识不再有默认值：`read_line` 必填，空输入重新询问。
- 连接测试不再可跳过（删除 `[Y/n]` 的「n」分支），失败时只允许「重试 / 返回上一步修改 / 稍后设置」，「稍后设置」不落盘。
- 「稍后设置」后仍然 `save_dismissed`；真正的失败拦截交给第 4 节的运行期检查。

### 3. `/v1` 提醒

口径（写死，避免与现有实现漂移）：

- 仅对 `provider == openai` 生效。
- 判定：`api_url` 去掉尾部空白与尾部 `/` 后，**不以 `/v1` 结尾**即提示。现有 `suspectsMissingV1`（`SettingsModelsTab.tsx:65-70`）的注释说"含 `/v1` 前缀即无虞"，实现却是 `endsWith("/v1")`，本次以实现口径为准并修正注释。
- 语义是**提醒**不是阻断：在向导与设置页都显示「OpenAI 兼容接口的地址通常以 `/v1` 结尾，请确认」，保存前需要用户二次确认（`window.confirm` 级别的确认即可）。
- 判定逻辑抽到 `desktop/src/lib/models.ts` 的一个纯函数（如 `looksMissingV1(provider, apiUrl)`），向导与 `SettingsModelsTab` 共用，避免两份启发式。

理由：`crates/yi-agent-llm/src/openai/client.rs:24,46-51,192-198` 把 `base_url` 当"含版本段的 API 根"，`/chat/completions` 直接拼接，**不会**自动补 `/v1`。写错时表现为 404，被 `openai/error.rs:27-31` 归为 `InvalidRequest`，而 `onboarding.rs` 的 `failure()` 会翻成"模型标识不被该地址接受"——这个提示会误导用户去改模型名。

### 4. 运行期：不再猜模型

`RuntimeConfig::load_inner`（`crates/yi-agent-runtime/src/config.rs:238-300`）：

- 保留 provider 的默认 `api_url`（`https://api.anthropic.com` / `https://api.openai.com/v1`）——地址有客观默认值。
- **移除** `default_model` 的兜底：无注入时 `model` 为空串。
- 不因此让加载失败：侧车必须能在未配置状态启动才能提供引导 RPC。空 model 只在使用点拦截。

使用点拦截（**显式校验，不靠匹配错误文本**）：

- `crates/yi-agent-runtime/src/bootstrap.rs` 的 `build_provider` 在 `cfg.model.trim().is_empty()` 时 `anyhow::bail!`，作为纵深防御（任何别的调用点也不会把空模型发给 provider）。措辞统一为「模型未配置：请在设置中完成模型配置」。
- 权威拦截点在 RPC 入口：`crates/yi-agent-app-server/src/rpc/thread.rs` 的 `"thread/start"` 在 `resolve_effective` 之后、`build_agent` 之前检查 `effective.model.trim().is_empty()`；为空则直接返回新增的 `RpcError::model_not_configured()`（数值码 `-32027`，`data.code = "model_not_configured"`，`data.reason` 取 `onboarding::assess` 的缺失项）。同一检查加到 `"thread/resume"`（`rpc/thread.rs` 的 resume 分支）。
  - 不用「让 `build_provider` 抛错再由 `RpcError::internal` 转译」的原因：错误文本是给人看的，会随文案调整漂移；UI 需要稳定词汇。
- `"thread/setModel"` 已有 `model_not_found` 校验（`rpc/thread.rs:798-808`），不改。
- `crates/yi-agent/src/main.rs:1213` 的 `unconfigured_guidance` 判据从"key 为空"扩展为"key 为空 **或** model 为空"，保证 CLI 给出同一条指引。

显式注入不受影响：`model_injected == true` 时 model 来自 `--model` / 进程环境变量，非空校验只在正常路径生效。

### 5. 设置页的默认条目

- `model/upsert`（`model_rpc.rs:202-300`）：在写入同一把锁里，若 `catalog.default_model.is_none()`，把新条目设为默认。响应增加 `default_model: Option<String>`。这修掉"新增了模型但新建会话仍报未配置"。
- `model/delete` 清空默认后（`model_rpc.rs:294-310`），界面必须可见：`model/list` 已返回 `default_model`（`model_rpc.rs:46`），`SettingsModelsTab` 在 `default_model === null && models.length > 0` 时显示「未设置默认模型」警告与一键设为默认。
- 不自动挑选"第一个条目"作为默认（删除是用户的显式动作，替他选择会掩盖影响）。

## 错误处理与边界

| 场景 | 表现 |
| --- | --- |
| 向导 model 为空 | 保存按钮禁用；不可提交 |
| 向导测试失败 | 只能重测或修改；不可完成 |
| RPC 直接 apply 且测试失败 | `-32602` + `data.code = "model_test_failed"`，零写入 |
| openai url 缺 `/v1` | 强提醒 + 二次确认；确认后仍可保存 |
| 无配置时新建会话 | `-32027` + `data.code = "model_not_configured"` |
| 无配置时 CLI 启动 | 打印统一指引并 `exit(1)` |
| 清单有模型但默认为空 | 设置页警告；新建会话报 `model_not_configured` |

风险与取舍：

- **`test_connection` 的探测请求可能被合法配置拒绝**：它用 `max_tokens = 1`（`onboarding.rs:305-312`），部分推理模型要求更小的 `max_completion_tokens` 或拒绝 `max_tokens`。硬门禁会把合法配置挡在外面。缓解：`failure()` 对 `InvalidRequest` 的措辞改为可归因的两种情况——「该地址不接受这个模型名」与「该模型拒绝了探测请求（请确认模型名与地址）」，并在 UI 上提供「这是探测请求被拒，不是配置错误」的说明与重试入口。**不**放宽门禁本身，因为用户明确要求"必须测试通过"。
- **探测不携带通道选择**：`test_connection` 构造 provider 时注释明确写着"探测不携带 api_style / reasoning_effort"（`onboarding.rs:288-291`）。对 `api_style = responses` 的模型，探针走 `auto`，可能测通一个真实会话走不通的通道，也可能测不通一个真实可用的配置。因此向导只测"key / url / model 三件套"，这是有意的范围限定：`api_style` 在引导阶段不暴露，用户可在设置页另行调整。此限定必须写进向导文案（「本次只验证地址、密钥与模型标识」）。
- 现有测试要改语义：`crates/yi-agent-runtime/src/config.rs:807-845`（`load_defaults_api_url_and_model`）与 `:1000-1015`（openai 默认值）目前断言默认 model，改为断言"url 有默认值、model 为空"。
- 前端 `OnboardingWizard.test.tsx` 的 5 个用例需要重写（默认回填、仍然保存两个行为都被删除）。

## 测试与验收

1. `OnboardingWizard`：model 为空时保存按钮 disabled；测试失败后仍 disabled；成功测试后才能完成。
2. `onboarding/apply` 在连接失败时返回 `model_test_failed` 且 `models.json` 不存在/未变（新单测：注入一个返回 401 的桩 provider）。
3. `openai` + `https://api.example.com` 触发提醒；`https://api.example.com/v1` 与 `https://api.example.com/v1/` 不触发。
4. `RuntimeConfig::load_lenient` 在无注入、清单为空时 `model == ""`，`api_url` 仍是 provider 默认。
5. `thread/start` 在无配置时返回 `-32027` / `model_not_configured`；`thread/resume` 同样返回该错误，且不落盘任何 meta。
6. `model/upsert` 在 `default_model == null` 时把新条目设为默认。
7. 命令：`cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`、`cargo test -p yi-agent-runtime --lib config`、`cargo test -p yi-agent-app-server onboarding`；`cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx src/components/SettingsModelsTab.test.tsx src/lib/models.test.ts`；`cargo fmt --all`.

## 与其它设计的关系

- 空状态引导（`2026-10-19-empty-session-guidance-design.md`）负责"跳过向导之后怎么把用户带回来配置"。
- 子 agent 非 git 降级（`2026-10-19-nongit-subagent-workspace-design.md`）不依赖本设计，但本设计移除默认 model 后，子 agent 的模型来源只剩清单条目或显式注入，二者一致。
