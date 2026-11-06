# magpie-agent-yi-agent 集成

## 模块说明

把 Yi Agent 作为 Magpie 的一个 **agent 插件** 接入：用户在 Magpie 的 Agents 页
选一个 Magpie 模型，插件就把该模型写进 Yi Agent 的模型目录 `~/.yi-agent/models.json`，
让 Yi Agent 经 Magpie 网关访问模型目录里的全部模型。断开后恢复原配置。

## 范围边界

**做什么：**
- Magpie agent 插件入口（`integrations/magpie-agent-yi-agent/yi-agent.agent.js`）
- npm 包 `@yi-agent/magpie-agent-yi-agent`（`integrations/magpie-agent-yi-agent/package.json`）
- 语法 + 行为 + 配置恢复测试（`integrations/magpie-agent-yi-agent/tests/agent.test.mjs`）
- 路径过滤的 CI（`.github/workflows/magpie-agent-yi-agent.yml`）+ 版本 gating 的 npm 发布

**不做什么：**
- 不进 Magpie 社区市场收录（团队自行发布不自动收录，需另向 magpie-community/plugins 提 PR）
- 不支持 `YI_AGENT_HOME` 自定义目录：插件声明的是字面量 `~/.yi-agent/models.json`，而 Magpie 的 `pluggedPath` 用 `os.Expand` 展开（`internal/agent/plugged.go`），未设置的变量会展开成空串，因此也不能声明 `$YI_AGENT_HOME/models.json`（未设置时会写到 `/models.json`）
- 不声明 `library.instructions` / `library.mcp` / `library.mcpFormat`：Yi Agent 只读项目级 `<workdir>/AGENTS.md`（`bootstrap.rs`，且**不**向上搜索父目录）与项目级 `<workdir>/.yi-agent/mcp.json`，两者都没有固定家目录路径，声明了只会写到 Yi Agent 永远不读的位置
- 不跟随 `YI_AGENT_SKILLS_DIR`：`library.skills` 固定声明默认根 `~/.yi-agent/skills`（`yi-agent-store/src/paths.rs` `skills_dir`）；设置了该变量的用户拿不到 Magpie 写的技能

## Features

- [x] Magpie agent 插件入口 — `integrations/magpie-agent-yi-agent/yi-agent.agent.js`：导出 `agent`（`id: "yi-agent"`、`name: "Yi Agent"`、`bin: "yi-agent"`、`config: "~/.yi-agent/models.json"`、`format: "json"`（显式钉住读取格式，不留给 `.json` 后缀推断）、`model: "default_model"`、`prefix: "magpie/"`、`ua: ["yi-agent"]`、`library: { skills: "~/.yi-agent/skills" }`、`icon`（与 `package.json` 的 `magpie.icon` 同一张 PNG 的 data URI））与 `connect(input)`。`connect` 是纯映射（Magpie agent 运行时 moejs 无 Node API、无 fetch、加载与 connect 各 1 秒）：每个 Magpie 模型映射为 `{name: "magpie/<id>", provider: "openai", api_url: gateway.v1, api_key: gateway.key, model: <id>, context_length?, api_style?, reasoning_capabilities?}`；`context_length` 仅在为有效正整数且 ≤ u32::MAX 时填入；`api_style` 在 Magpie 报告原生 `/chat/completions` 端点时写 `"chat"`，否则不写（留 `auto`）；`reasoning_capabilities` 把 `model.efforts`（过滤到 yi-agent 七档白名单 `none/minimal/low/medium/high/xhigh/max`，`probed_at: 0`）写到 `api_style` 决定的通道（chat 或 responses）上，`efforts` 为空/不可用时省略；`subagent_model` 在连接期间指向当前 Magpie 模型。`library.skills` 复用 yi-agent 已存在的用户技能根 `~/.yi-agent/skills`。判据：`cd integrations/magpie-agent-yi-agent && node --test tests/agent.test.mjs`（23 passed；不用 glob，`node --test` 的 glob 支持是 Node 21+ 才有，CI 固定 Node 20）
- [x] npm 包声明 — `integrations/magpie-agent-yi-agent/package.json`：`name: "@yi-agent/magpie-agent-yi-agent"`、`magpie.agent: "./yi-agent.agent.js"`、`type: module`、`files: [yi-agent.agent.js, README.md]`、无 `main`/`exports`（避免 Magpie 把 `connect` 当 provider 入口重复加载）。判据：`node --check integrations/magpie-agent-yi-agent/yi-agent.agent.js` 通过
- [x] 配置恢复与同步测试 — `integrations/magpie-agent-yi-agent/tests/agent.test.mjs`：覆盖无配置文件、已有模型配置 + 无关顶层键保留、切换默认模型、Magpie 模型增删、重复 `connect()` 确定性、断开恢复、`subagent_model` 有值/为空、异常模型元数据（`context` 缺失/字符串/0/负/超 u32/非整数）、`api_style`（`native_endpoints` 含 `/chat/completions` → `chat`，`/responses` `/messages` 或缺失 → 不写）、`reasoning_capabilities`（`efforts` 映射到 chat/responses 通道、`ultra` 等白名单外值被过滤、`probed_at: 0`、空/不可用 efforts 省略）、`default_model` 与 `models[].name` 精确一致、`format: "json"` 与 `library` 只含 `skills` 的声明守卫、agent 导出 `icon` 为合法 PNG data URI 且与 `package.json` 的 `magpie.icon` 逐字节相同。写协议模拟按 Magpie 文档「备份 → 写 → 恢复」逐 key 建模。判据：`node --test`（23 passed）
- [x] CI 检查脚本 — `scripts/test-magpie-agent-yi-agent.sh`：语法检查（`node --check`）+ 行为测试（`node --test`），缺 Node 时 exit 2（基础设施故障而非假绿）。刻意**不进** `just ci`：GitLab MR 的 `rust:latest` 镜像没有 Node。判据：`bash scripts/test-magpie-agent-yi-agent.sh`（exit 0）、`bash scripts/test_test_magpie_agent_yi_agent.sh`（2 passed，含缺 Node → exit 2）
- [x] 路径过滤的 CI + npm 发布 — `.github/workflows/magpie-agent-yi-agent.yml`：PR 且 `integrations/magpie-agent-yi-agent/**` 变化跑 CI；push 到 `main` 且该目录变化跑 CI 后按版本 gating 发布 npm（已发布版本自动跳过，版本与主包独立）。判据：workflow 存在且 `justfile` 的 `magpie-agent-yi-agent-check` recipe 可本地复用
- [x] 版本兼容声明与发布策略 — `package.json` 新增 `yiAgent: { minVersion: "0.1.4", fullFieldsSince: "0.1.8", verifiedWith: "0.1.13" }`（Magpie 与 npm 都忽略未知顶层字段，故仅为语义元数据；插件运行在 moejs 里没有文件系统/进程访问，无法探测已装版本，只能靠发布流程守约）。`README.md` 新增 `## Version compatibility`：逐字段给出最低版本（`models.json` 与 `models`/`default_model`/`subagent_model` 自 v0.1.4；`api_style` v0.1.5；`context_length` v0.1.7；`reasoning_capabilities` v0.1.8），说明未知字段被静默忽略（`ModelEntry` 无 `deny_unknown_fields`，v0.1.4–v0.1.13 实测均为 0 处）所以单一插件包向前兼容全部版本，并点名两处真实退化：0.1.7 之前不读 `context_length`（压缩阈值固定 200000 = `config.rs:347` 的 `unwrap_or(200_000)`，小窗口模型会超长被拒）、0.1.8 之前不读能力集（走剥离重投兜底）。同章给出四场景决策表：插件自身适配 bug → patch 单独发；新增已发布的字段支持 → patch；跟随新字段 → minor 同周期发；`verifiedWith` 随之抬升；Yi Agent 更新但插件无需改 → 插件不发包。判据：`node --test tests/agent.test.mjs`（23 passed），新增 3 例守卫——`manifestFields()` 实测 `connect()` 产出字段集合必须被声明覆盖、`fullFieldsSince` 必须等于可选字段中最新版本、README 必须逐字引用三个声明值且列出每个可选字段及其版本；改错任一数字即红
- [x] 字段命名规范（四种写法各归其位）— 元数据键定为 **`yiAgent`**，因为它是 **JSON 键**而不是包名或展示名；package.json 自身的多词键一律 camelCase（`devDependencies` / `peerDependencies` / `publishConfig` / `optionalDependencies`），带连字符的顶层键会是全文件唯一异类。四种写法对应四种标识符，不可互换：`@yi-agent/yi-agent` = npm 包名（注册表 id，决定装什么）、`Yi Agent` = 展示名（`agent.name`、文档、UI）、`yi-agent` = 机器标识（`agent.id`、`bin`、`~/.yi-agent`、仓库名、`YI_AGENT_*` 环境变量前缀）、`yiAgent` = JSON 键。两处**不能**放：(1) 不塞进 `magpie` 对象——那是 Magpie 的命名空间，Magpie 文档原文是「OpenCode ignores the `magpie` object, so these are safe to add」，占用它等于冒领别的项目的键；(2) 不用 `engines`——它只能表达单个 range，且包管理器**被允许强制执行**，而本插件由 Bun 通过 `bun add` 安装，未来某版本若真的校验未知 engine 键，会把「兼容性说明」变成「安装失败」，没人读的键才不可能失败。实测 `npm pack` 会把 `yiAgent` 与 `magpie` 两个自定义块完整保留在 tarball 里，发布不会剥掉。判据：`node --test tests/agent.test.mjs`（23 passed），新增 1 例守卫 `each identifier uses its own spelling` 断言四个角色各自的拼写、`magpie` 的键必须恰好是 `["agent","icon"]`、`engines` 必须为 undefined

## 参考

- [Magpie agent 插件接口](https://usemagpie.ai/docs/plugins#agent-plugins)：agent 声明字段、`connect(input)` 的 `gateway`/`model`/`models`/`agent`、moejs 受限运行时、逐 key 备份/恢复
- Yi Agent 模型目录契约：`yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（`ModelEntry` / `ModelCatalog` / `models_path`）
