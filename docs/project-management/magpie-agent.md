# magpie-agent 集成

## 模块说明

把 Yi Agent 作为 Magpie 的一个 **agent 插件** 接入：用户在 Magpie 的 Agents 页
选一个 Magpie 模型，插件就把该模型写进 Yi Agent 的模型目录 `~/.yi-agent/models.json`，
让 Yi Agent 经 Magpie 网关访问模型目录里的全部模型。断开后恢复原配置。

## 范围边界

**做什么：**
- Magpie agent 插件入口（`integrations/magpie/yi-agent.agent.js`）
- npm 包 `@yi-agent/magpie-agent`（`integrations/magpie/package.json`）
- 语法 + 行为 + 配置恢复测试（`integrations/magpie/tests/agent.test.mjs`）
- 路径过滤的 CI（`.github/workflows/magpie-agent.yml`）+ 版本 gating 的 npm 发布

**不做什么：**
- 不进 Magpie 社区市场收录（团队自行发布不自动收录，需另向 magpie-community/plugins 提 PR）
- 不声称支持 `YI_AGENT_HOME` 自定义目录（Magpie 的 `$VAR` 展开行为未实测，默认只支持 `~/.yi-agent/models.json`）

## Features

- [x] Magpie agent 插件入口 — `integrations/magpie/yi-agent.agent.js`：导出 `agent`（`id: "yi-agent"`、`name: "Yi Agent"`、`bin: "yi-agent"`、`config: "~/.yi-agent/models.json"`、`model: "default_model"`、`prefix: "magpie/"`、`icon`（与 `package.json` 的 `magpie.icon` 同一张 PNG 的 data URI））与 `connect(input)`。`connect` 是纯映射（Magpie agent 运行时 moejs 无 Node API、无 fetch、加载与 connect 各 1 秒）：每个 Magpie 模型映射为 `{name: "magpie/<id>", provider: "openai", api_url: gateway.v1, api_key: gateway.key, model: <id>, context_length?}`；`context_length` 仅在为有效正整数且 ≤ u32::MAX 时填入（否则 Yi Agent 的 `Option<u32>` 反序列化会拒掉整条）；`subagent_model` 在连接期间指向当前 Magpie 模型。判据：`cd integrations/magpie && node --test "tests/**/*.test.mjs"`（14 passed）
- [x] npm 包声明 — `integrations/magpie/package.json`：`name: "@yi-agent/magpie-agent"`、`magpie.agent: "./yi-agent.agent.js"`、`type: module`、`files: [yi-agent.agent.js, README.md]`、无 `main`/`exports`（避免 Magpie 把 `connect` 当 provider 入口重复加载）。判据：`node --check integrations/magpie/yi-agent.agent.js` 通过
- [x] 配置恢复与同步测试 — `integrations/magpie/tests/agent.test.mjs`：覆盖无配置文件、已有模型配置 + 无关顶层键保留、切换默认模型、Magpie 模型增删、重复 `connect()` 确定性、断开恢复、`subagent_model` 有值/为空、异常模型元数据（`context` 缺失/字符串/0/负/超 u32/非整数）、`default_model` 与 `models[].name` 精确一致、agent 导出 `icon` 为合法 PNG data URI 且与 `package.json` 的 `magpie.icon` 逐字节相同。写协议模拟按 Magpie 文档「备份 → 写 → 恢复」逐 key 建模。判据：`node --test`（14 passed）
- [x] CI 检查脚本 — `scripts/test-magpie-agent.sh`：语法检查（`node --check`）+ 行为测试（`node --test`），缺 Node 时 exit 2（基础设施故障而非假绿）。刻意**不进** `just ci`：GitLab MR 的 `rust:latest` 镜像没有 Node。判据：`bash scripts/test-magpie-agent.sh`（exit 0）、`bash scripts/test_test_magpie_agent.sh`（2 passed，含缺 Node → exit 2）
- [x] 路径过滤的 CI + npm 发布 — `.github/workflows/magpie-agent.yml`：PR 且 `integrations/magpie/**` 变化跑 CI；push 到 `main` 且该目录变化跑 CI 后按版本 gating 发布 npm（已发布版本自动跳过，版本与主包独立）。判据：workflow 存在且 `justfile` 的 `magpie-agent-check` recipe 可本地复用

## 参考

- [Magpie agent 插件接口](https://usemagpie.ai/docs/plugins#agent-plugins)：agent 声明字段、`connect(input)` 的 `gateway`/`model`/`models`/`agent`、moejs 受限运行时、逐 key 备份/恢复
- Yi Agent 模型目录契约：`yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（`ModelEntry` / `ModelCatalog` / `models_path`）
