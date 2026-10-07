# 模型设置（Models tab + 会话模型下拉）设计

日期：2026-10-04
状态：待评审

## 1. 背景与问题

今天「用哪个模型」是**一台机器一份全局配置**：

- provider / api_url / api_key / model 来自 `.env`（`MODEL_API_KEY`、`MODEL_API_URL`、
  `YI_AGENT_MODEL`、`YI_AGENT_PROVIDER`）加 CLI 覆盖，由
  `yi-agent-runtime/src/config.rs::RuntimeConfig::load` 合并。
- provider 只在会话建立时按这份 cfg 造一次
  （`yi-agent-runtime/src/bootstrap.rs::build_provider`，只读 `cfg.provider` /
  `cfg.api_url` / `cfg.api_key` / `cfg.model`）。
- 设置界面（`desktop/src/components/SettingsDialog.tsx`）只有 通用 / 远程访问 / 插件
  三个 tab，没有任何模型相关设置。

用户需求：

1. 设置里新增一个 **模型 tab**，能添加模型条目，每条含 **url、model name、provider 格式**
   （讨论后确认还需 **显示名** 与 **API key**）。
2. UI 里**每个会话都有一个可选当前模型的下拉**。

讨论中敲定 `provider 格式` 是**协议格式枚举**（`anthropic` | `openai`），不是供应商品牌，
正好对上 `build_provider` 现成的两个分支。API key 必须**跟随条目**（url 变了 key 通常也要变），
不靠 `.env` 里的全局 key。

## 2. 决策记录（本次头脑风暴敲定）

| 问题 | 决定 |
| --- | --- |
| API key 归属 | **每条自带**。key 跟 url 走；不再依赖 `.env` 单一全局 key。 |
| provider 格式 | **枚举两个协议**：`anthropic` / `openai`（对齐 `build_provider` 现有分支）。 |
| 清单存储位置 | **机器级全局** `~/.yi-agent/models.json`（与已有 `~/.yi-agent/.env` 同目录），**不是**项目的 `preferences.json`。 |
| 会话选模型的语义 | **全局默认 + 会话覆盖**：清单标一个全局默认；会话一旦手动选过即固定（持久到 thread meta），此后每轮都用它。 |
| 切换时机 | 会话正在跑 turn 时改模型 → **本轮结束后生效，不打断**；不禁用下拉。 |
| 旧 meta 模型名对不上清单 | **回退到全局默认**，不报错。 |
| 底层生效方式 | **重建该会话的 agent**（复用 CLI `/model` 的 `rebuild_driver_agent` 机制），保留 session 历史。 |
| 下拉展示与指认 | 用**显示名**做唯一键（同名条目写入时拒绝），下拉显示名可附 model 作副标题。 |
| 子 agent 模型 | **单独设置**（`subagent_model`），**全局一份**；改完**下次项目 daemon 重启生效**；未设置回退全局默认。 |
| 输入入口位置 | **输入框右下角一个可点击模型框**（点开是清单下拉）。 |
| 状态栏 model 文本 | **移除**（入口已在输入框右下角，避免两处显示同一东西）。 |
| 覆盖端 | **全栈**：宿主 RPC + 桌面端 + TUI。 |

## 3. 目标与非目标

**目标**

- 设置里新增 **模型 tab**：增删改模型条目（显示名 / provider 格式 / url / model name /
  api key），并设置「全局默认模型」与「子 agent 模型」。
- 会话级模型选择：输入框右下角下拉，可给当前会话选模型，也可一键回到默认；选择持久。
- 模型清单机器级共享，跨项目、跨重启保留。
- TUI 能列清单、能按显示名给当前会话选模型。

**非目标**

- 不做 OAuth / 登录式鉴权（只支持 api key）。
- 不引入 `anthropic` / `openai` 之外的第三套协议格式。
- 不改动 `.env` 的角色：它仍作为「清单为空时的兜底」，保证零配置仍能启动。
- 不做「每个子 agent 各自选模型」（子 agent 模型是全局一份）。
- 不做凭据的加密存储（v1 明文 + 文件权限收紧，见 §9）。
- 不做插件式 provider 注册（第三协议格式需改代码，不是配置）。

## 4. 数据模型与存储

### 4.1 文件

`~/.yi-agent/models.json`（`$HOME` 缺失时回退当前目录，与 `resolve_global_env_path` 同口径）：

```jsonc
{
  "models": [
    {
      "name": "公司-内网",             // 显示名，唯一、非空；会话覆盖指认它
      "provider": "anthropic",        // 枚举：anthropic | openai
      "api_url": "https://llm.corp/v1",
      "model": "claude-sonnet-4-5",   // 传给 provider 的 model 串
      "api_key": "sk-..."             // 明文存盘，文件 0600
    }
  ],
  "default_model": "公司-内网",        // 全局默认；null/缺失 = 未见配置
  "subagent_model": "公司-内网"       // 子 agent 模型；null/缺失 = 回退全局默认
}
```

### 4.2 不变式

1. **显示名唯一**。同名 upsert 视为更新；两条同名不可能并存。
2. `default_model` / `subagent_model` 存的是**引用（显示名）**，不是快照。
3. **悬空引用一律回退、不报错**：
   - `default_model` 指向不存在的条目 → 回退到 `.env` / `cfg` 的 provider+model+url+key；
   - `subagent_model` 悬空或缺失 → 回退 `default_model`（其再悬空则再回退 `.env`）。
4. 条目字段缺失/类型错误的一律**跳过该条**并 `tracing::warn!`（坏清单不阻断启动，与
   `settings_store` 对 preferences 的容错同约定）；整个 `models.json` 不可解析时当作
   「空清单 + 无默认」，同样回退 `.env`。
5. api_key 允许为空串（有些自建网关不校验）；但**读接口绝不回传明文**（见 §6）。

### 4.3 落盘约定

沿用 `yi-agent-app-server/src/settings_store.rs` 的既有约定：

- 读-改-写，保留无关顶层键；
- 临时名逐次唯一（`models.json.<pid>.<seq>.tmp`），最后 `rename` 原子替换；
- 首次创建即以 `0600` 打开（`OpenOptions::mode(0o600)`），已存在的文件在每次写入后
  `set_permissions(0o600)`。

### 4.4 放置在哪个 crate

读写逻辑放 **`yi-agent-runtime`（共享 crate）** 的新模块 `models.rs`，暴露：

- `ModelStore`（load/save 上述结构，含校验）；
- `resolve_effective(cfg, session_override: Option<&str>) -> RuntimeConfig`
  （见 §5）。

理由：**TUI 不经 app-server**（它是 `yi-agent` 进程自己跑 agent），必须能直接读这份清单；
app-server 与 CLI 也都需要它。只挂 app-server 无法满足全栈。

## 5. 生效路径

### 5.1 会话级模型解析

新增纯函数（`yi-agent-runtime/src/models.rs`）：

```
resolve_effective(cfg: &RuntimeConfig, session_override: Option<&str>) -> RuntimeConfig
```

优先级：

1. 会话覆盖（`session_override`，即 thread meta 的 `model_ref`，一个**显示名**）且该显示名
   仍在清单 → 用该条目的 provider / api_url / api_key / model 覆盖 `cfg` 的对应字段；
2. 否则 `default_model` 指认的条目（仍在清单）；
3. 否则**原样返回 `cfg`**（即今天 `.env` 的行为）。

返回的是 `RuntimeConfig` 的克隆，只有 provider/api_url/api_key/model 四个字段被改写，
其余（workdir、sandbox、预算…）保持 `cfg` 原值。

**ThreadMeta 的两个字段（避免改旧字段语义）**：

- 新增 `model_ref: Option<String>`：会话覆盖，存**显示名**（`None` = 跟随全局默认）。
- 保留既有 `model: String`：**当前生效的 model 串**（用于显示 / 通知），不再兼职存覆盖。
  它在 `thread/start`、`thread/resume`、切模型成功后由 `resolve_effective` 的结果写回。

这样老 meta 里已有的 `model` 字符串语义不变（仍是生效串），不会被误当成显示名去清单里找。

### 5.2 `build_agent` 接缝（核心工作量）

现状：app-server 的 `build_agent` 闭包签名是
`Fn(Option<Session>, &Path, ThreadMode) -> BuiltAgent`（`server.rs:1489` 附近），
**拿不到「这个会话该用哪条模型」**，provider 只能按全局 `cfg` 造。

改动：让会话建立/重建时用「该会话解析出的 cfg」去 `build_provider`。两种接法二选一，
实现计划里定：

- **(a)** 把闭包签名扩一个参数（解析好的 `RuntimeConfig` 或 `Option<&str>` 覆盖名）；
- **(b)** 把 provider 构造从闭包里移出，由服务端在拿到会话覆盖后自行组装。

无论哪种，`thread/start`、`thread/resume`、以及「切模型重建」三条路径都必须走
`resolve_effective` 再建 agent，语义才一致。

### 5.3 切模型 = 重建该会话 agent

`thread/setModel { thread_id, name }`：

1. 校验 `name` 指向已存在条目（`null` = 清覆盖）；
2. 写 thread meta：`model_ref = name`（或 `None` 清覆盖），并按 `resolve_effective` 的结果
   写回生效 `model`（沿用 `thread_store` 的原子写）；
3. 通过会话命令通道（现成 `SessionCommand::{Clear,Compact}`，`server.rs:4764`）新增
   `SessionCommand::SetModel` 变体，交给 thread driver；
4. driver 在两轮之间用新的会话覆盖重建 agent（保留同一个 session Arc / 历史），
   重建后发 `AgentEvent::ModelChanged { model }`——TUI 侧已有对该事件的处理
   （`tui/history.rs:977`），桌面端需要新增对应渲染。
5. 本轮在跑时：命令排到队尾，本轮结束后生效，不打断。

> 这与 CLI 里 `ControlCommand::SetModel`（`yi-agent/src/main.rs:1972`）是同一机制，
> 只是承载通道从 TUI 的 control channel 换成 app-server 的 session channel。

### 5.4 子 agent 模型

- daemon 的 worker 工厂（`yi-agent-subagent/src/attach.rs:84::worker_factory`）按
  `subagent_model`（缺省回退全局默认，再缺省回退 `.env`）解析出的 cfg 去
  `build_provider`；worker 的 `config.model` 相应改为该条目的 model。
- **作用时机**：改完 `subagent_model` **下次项目 daemon 重启生效**，UI 明示这句；
  v1 不热重建 daemon（不动 daemon 生命周期机制）。
- `worker_config_model` 仍忽略编排 agent 传入的 model 字符串（现状语义保留），
  它取的是工厂自己的条目 model。

## 6. 宿主 RPC（app-server）

新增 `model/*` 命名空间，**机器级全局、不带 project 参数**（与 `ui/settings/*` 同类，
读写 `~/.yi-agent/models.json`）。

| 方法 | 参数 | 返回 |
| --- | --- | --- |
| `model/list` | 无 | `{ models: [...], default_model, subagent_model }` |
| `model/upsert` | `{ name, provider, api_url, model, api_key? }` | `{ ok: true }` |
| `model/delete` | `{ name }` | `{ ok: true }` |
| `model/setDefault` | `{ name \| null }` | `{ ok: true }` |
| `model/setSubagent` | `{ name \| null }` | `{ ok: true }` |
| `thread/setModel` | `{ thread_id, name \| null }` | `{ ok: true }` |

**读接口的 key 处理**：`model/list` 对每条返回 `has_key: bool` 与
`api_key_masked: string`（形如 `••••abcd`，只留尾 4 位），**绝不回传明文 `api_key`**。

**`model/upsert` 的 key 语义**：

- `api_key` 字段**缺省 = 保留原值**（UI 只显示掩码，用户不重输就不会把 key 抹掉）；
- `api_key` 传空串 = 清空该条的 key；
- 传非空串 = 覆盖。

**校验（写入前全量校验，非法则一个字节不落盘，沿用既有「拒绝即零落盘」约定）**：

- `name` 非空；`provider` ∈ {`anthropic`,`openai`}；`api_url`、`model` 非空；
- `setDefault`/`setSubagent`/`thread/setModel` 的 `name` 必须指向已存在条目，否则
  结构化错误 `model_not_found`。

**错误码**：新增 `model_not_found`（`data.code`，落在新的数字码，如 `-32025`），
沿用 `board_query` 的做法——数字码只是粗回退，`data.code` 才是稳定词表，前端只读
`data.code`（`protocol.rs:136`）。

**权限**：`model/list` 需 `Observe`；所有写操作需 `Control`（与既有写操作同档）；
`thread/setModel` 作用于某会话，需 `Control` 且该会话存在（否则 `unknown_thread`）。

## 7. 桌面端

### 7.1 设置 · 模型 tab

- `SettingsDialog.tsx` 的 `TABS` 增加 `{ id: "models", label: "模型" }`，并渲染新面板。
- 新增 `SettingsModelsTab.tsx`：
  - 清单列表（每行：显示名 / provider / model / url / key 掩码），可增删改；
  - 「全局默认模型」下拉、「子 agent 模型」下拉（含「跟随全局默认」项）；
  - 走 `model/*` RPC（宿主注入的 `modelCall` 接缝，风格同 `pluginCall`）；
  - 写成功后**以前端重读的结果为准，不做乐观更新**（与插件设置同一约定）；
  - 写失败内联报错，保留用户输入；
  - key 输入框：显示掩码，用户不改则不发送 `api_key` 字段。

### 7.2 会话模型下拉

- 在 `MessageInput` 右下角新增一个可点击的**模型框**（显示当前会话生效的 model 名），点开是
  一个下拉：顶部「跟随全局默认 · <解析后的 model 名>」，下面列各条显示名；
  选条 = `thread/setModel`，选默认项 = `thread/setModel { name: null }`。
- 下拉数据来自 `model/list` + 当前会话 meta（`info.model` 显示生效串、`info.model_ref`
  判定当前是否正选中某条及选中哪条）。
- 组件放 `ModelPicker.tsx`，纯逻辑（选项构造、当前选中判定）抽到
  `desktop/src/lib/modelPicker.ts` 以便单测，风格同 `boardIndex.ts`。

### 7.3 StatusBar

- **移除** model 文本（`StatusBar.tsx:32`），含其 `model` prop。状态栏保留连接状态、
  cwd、用量。模型只在输入框右下角出现。

### 7.4 协议封装

新增 `desktop/src/lib/models.ts`：`listModels` / `upsertModel` / `deleteModel` /
`setDefaultModel` / `setSubagentModel` / `setThreadModel`，及把错误映射到
`not_found` / `other` 的纯函数。

## 8. TUI

- TUI 有自己的 agent 进程，**直接**用 `yi-agent-runtime::models` 读清单，不经 app-server。
- `/model` 升级：无参时**列清单**（显示名 + 当前会话选中项 + 全局默认）；带参
  `name` 时把**当前会话**切到该显示名（走 §5.3 的 `ControlCommand::SetModel` 路径，
  但模型名换成解析后的 cfg）；`/model default` 清除会话覆盖。
- 新增 `/models`（或并入 `/model`）：列出清单全部条目（只读展示）。
- TUI 的「会话」只活在进程生命周期内（CLI 不持久化 thread meta），故 TUI 里的会话覆盖
  进程结束即消失；TUI 不写 thread meta。这是已接受的范围界定。

## 9. 错误处理与安全

- **密钥**：`models.json` 明文存盘，权限 `0600`；`model/list` 不回传明文；UI 默认掩码；
  日志不得打印 `api_key`（校验失败的错误消息里也不带 key 值）。
- **坏文件**：不可解析 → 视为空清单，回退 `.env`，`tracing::warn!`。
- **悬空引用**：`default_model` / `subagent_model` / 会话 meta 指向的显示名不存在 →
  回退（见 §4.2），不报错。
- **写失败**：内联报错、零落盘、不改前端显示值。
- **切模型时 provider 构造失败**（如 url 非法）：该次重建失败，会话**保留原模型**并
  返回结构化错误；不得留下半重建状态（沿用既有重建的原子性）。
- **权限**：写操作 `Control`；`Observe` 级设备只能读 `model/list`（且只看到掩码）。

## 10. 测试

**Rust（runtime）**

- `models.rs`：load/save 往返；同名拒绝；悬空 `default_model`/`subagent_model` 回退；
  坏文件回退空清单；写文件权限为 `0600`；写不留临时文件；保留无关键。
- `resolve_effective`：会话覆盖命中 / 悬空回退默认 / 默认悬空回退 cfg 三条路径；
  只改四个字段，其余字段等于 `cfg`。

**Rust（app-server）**

- `model/list` 不回传明文 key（断言响应里不含 key 原文）、掩码正确；
- `model/upsert` 的 key 缺省保留 / 空串清空 / 非空覆盖三种语义；
- 校验失败零落盘（读回旧值不变）；
- `setDefault`/`setSubagent`/`thread/setModel` 指向不存在条目 → `model_not_found`；
- `thread/setModel` 写 thread meta，并在 turn 结束后生效（沿用既有 driver 测试桩）；
- 切模型重建保留 session 历史（断言重建后历史未丢）。

**Rust（subagent）**

- `worker_factory` 按 `subagent_model` 解析 provider/model；未设置回退全局默认。

**桌面端**

- `SettingsDialog`：第四个 tab 存在、可切、aria 关系正确；
- `SettingsModelsTab`：增删改调用正确；key 掩码显示；不改 key 时不发送 `api_key`；
  写失败内联报错且不清空输入；
- `ModelPicker`：选项构造（含默认项显示解析后 model 名）；选中调用 `thread/setModel`；
- `modelPicker.ts` 纯函数；
- `StatusBar` 不再渲染 model 文本。

**TUI**

- `/model` 无参列清单；带参切当前会话；`/model default` 清覆盖。

## 11. 迁移与兼容

- 未配置 `models.json` 时行为**与今天完全一致**（走 `.env` / cfg），零配置仍能启动。
- thread meta **新增 `model_ref`**（`Option<String>`，`#[serde(default)]`，缺省 `None`），
  旧 meta 无此字段 → 无覆盖、跟随全局默认。既有 `model` 字段语义保持不变（生效串），
  故旧 log/meta 无需迁移。
- `thread/start` / `thread/resume` 响应与通知继续带 `model` 字段，语义不变（会话当前
  生效的 model 串）；新增 `model_ref` 在 `thread/list` 的会话元信息里暴露，供下拉回显
  被选中的显示名。
- `preferences.json` 不变（模型不走它）。
- 更新 `docs/project-management/desktop.md`、`yi-agent-app-server.md`、`yi-agent-runtime.md`
  登记新 RPC / 新设置项，并同步 `README.md` 索引计数（若涉及）。

## 12. 未决 / 风险

- **`build_agent` 接缝改法**（§5.2 (a) 还是 (b)）留给实现计划，但必须保证
  `thread/start`、`thread/resume`、切模型三条路径一致。
- **切模型重建期间正在跑的 turn**：命令排队到队尾，需在实现中确认 driver 的会话命令
  处理点确实在两轮之间（若某轮异常退出未清队列，要有兜底，避免命令丢失）。
- **api_key 明文**：v1 已知取舍（文件 `0600` + 读接口掩码）；后续可加 keyring 集成。
- **子 agent 模型需重启 daemon**：v1 接受；热重建 daemon 不在本期。
- **TUI 无 thread 持久化**：TUI 的会话覆盖不落盘，进程退出即失；这是现状使然，非本期目标。
- **同名显示名重命名**：本期对「改显示名」按「新增一条 + 删旧一条」处理，不做引用迁移
  （用户改名前需自行确认无引用，或 UI 提示会影响哪些引用）。若实现成本低，可在计划里
  升级为「重命名并迁移 default/subagent 引用」。
