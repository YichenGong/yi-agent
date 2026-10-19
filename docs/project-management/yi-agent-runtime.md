# yi-agent-runtime（共享运行时）

## 模块说明

`yi-agent-runtime` 是配置加载与 Agent 装配的共享 crate，供 CLI（`yi-agent`）与
桌面 app-server（`yi-agent-app-server`）复用，避免两处装配逻辑漂移。它从
`crates/yi-agent` 的 `config.rs` / `main.rs` 抽取而来，但**不依赖 clap**——CLI 负责
把 clap 解析结果转成 `ConfigOverrides` 再交给本 crate，因此 GUI 侧无需引入 CLI
参数解析。

## 范围边界

**做什么：**
- 配置加载：显式 override > 真实 env > `workdir/.yi-agent/.env` > `~/.yi-agent/.env` > 默认值
- provider 构造（anthropic / openai）
- 工具集装配：内置工具 + 进程工具（`ProcessManager`）+ skills + 系统提示词（含当前日期与 skills catalog）
- Agent 装配：provider + 工具 + 权限检查器 + 权限决定通道
- 面向 GUI 的脱敏配置视图

**不做什么：**
- 不做 clap 参数解析（生产依赖中不含 clap，仅 dev-dependency 用于编译器守卫）
- 不做 TUI（由 yi-agent-tui 负责）
- 不做 app-server 协议与传输（由 yi-agent-app-server 负责）
- 不做会话持久化（YAGNI，路线图 P1）

## Features

- [x] crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-runtime/`；`Cargo.toml` members 含 `crates/yi-agent-runtime`
- [x] `RuntimeConfig` + `ConfigOverrides` + `load()`（env/.env 层级合并，不依赖 clap）— `src/config.rs:13` / `src/config.rs:34` / `src/config.rs:169`
- [x] sandbox 手写解析 + 变体漂移编译器守卫 — `src/config.rs:64`，测试 `src/config.rs:486`
- [x] `redacted_view()`（api_key 脱敏，供 GUI `config/read`）— `src/config.rs:326`
- [x] `build_provider()`（anthropic / openai）— `src/bootstrap.rs:11`
- [x] `build_tool_setup()` / `build_tools()` / `build_system_prompt()`（内置工具 + 进程工具 + skills + 当前日期；`naked` 返回空 registry + `None` prompt）— `src/bootstrap.rs:68`
- [x] `bootstrap_agent()` + `PermissionMode` + 权限决定通道（`Interactive` 保留双向通道，`AutoAllow` 关闭通道使黑名单命令解析为 Deny）— `src/bootstrap.rs:182`
- [x] `AgentBootstrap` 暴露装配出的 provider / 工具注册表 / skills catalog 句柄 — `AgentBootstrap.provider`（`src/bootstrap.rs:252`）与 `.tools`（`:257`）/ `.catalog`（`:264`）；调用方用它们**重建** agent（换工具集与权限根）而不重新构造 provider——重建 provider 会重读凭据并多出一个 client。验证：`cargo test -p yi-agent-runtime --lib bootstrap::tests::bootstrap_exposes_the_provider_registry_and_catalog_handle`
- [x] 装配辅助 API：`build_prompt_setup()`（skills + system prompt）、`build_tool_setup_in()`（指定 workspace 根）、`build_agent_config()`、公开 `load_permission_checker()` — `src/bootstrap.rs:41` / `src/bootstrap.rs:97` / `src/bootstrap.rs:166` / `src/bootstrap.rs:231`
- [x] CLI 装配迁移到本 crate — `crates/yi-agent/src/main.rs` 的 provider（4 处：`control_schedule` / daemon worker / TUI / headless）、skills + system prompt、工具集、AgentConfig 装配全部委托本 crate，消除重复 — 验证：`cargo test -p yi-agent --bin yi-agent`（350 个测试）
- [x] 沙箱可提权标记 + bootstrap 暴露共享 `YoloSwitch` — `RuntimeConfig.sandbox_promotable`（`src/config.rs:28`）：显式 `--sandbox` / `YI_AGENT_SANDBOX` / `--yolo` / `--dangerously-skip-permissions` 时置 `false`（`src/config.rs:287`），默认 `true`；`build_tool_setup_with_switch` 用该值构造 `SandboxController::new(switch, cfg.sandbox, cfg.sandbox_promotable)`（`src/bootstrap.rs:162` / `:185`）；`AgentBootstrap.yolo` 暴露与 `PermissionChecker` 同一 `YoloSwitch`（`src/bootstrap.rs:233`），app-server 据此按线程翻转；验证：`cargo test -p yi-agent-runtime --lib config::tests::sandbox_promotable_` 与 `cargo test -p yi-agent-runtime --lib bootstrap` — [设计](../plans/2026-09-27-desktop-yolo-mode-design.md)

- [x] 机器级模型清单 + 会话级模型解析 — 新增 `src/models.rs`：`ModelProvider`（`src/models.rs:18`，枚举 anthropic|openai）、`ModelEntry`（`:41`）、`ModelCatalog`（`:61`）读写 `~/.yi-agent/models.json`——`models_path`（`:71`）按 `$HOME` 定位、`load_catalog_from`（`:86`）/ `save_catalog_to`（`:138`）读-改-写、临时名逐次唯一、`rename` 原子替换、文件收紧 `0600`，坏条目/整文件不可解析则跳过回退空清单，同名条目拒绝；纯函数 `resolve_effective(cfg, session_override)`（`:209`）按「会话覆盖 > 全局默认 > 原样 cfg」改写 provider/api_url/api_key/model 四字段，`effective_entry`（`:228`）与 `subagent_entry`（`:239`）分别取默认条目与 `subagent_model`；`mask_key`（`:198`）只留尾 4 位；未配置时行为与 `.env`/cfg 完全一致。验证 `cargo test -p yi-agent-runtime --lib models::`（13 例：落盘往返 / 0600 / 不留临时文件 / 坏文件回退 + resolve_effective 三条路径与「只改四字段」）

- [x] 首次安装引导共享逻辑（就绪判定 / 行级保留式 `.env` 写入 / 连接测试 / 结束标记 / 宽松加载）— 新增 `src/onboarding.rs`：`assess`（按 `.env` 字段 + 可选清单判定 `Ready`/`Needed`，纯函数）、`read_env_fields`、`test_connection`（临时 provider 发一条最小请求，把 `ProviderError` 映射成中文原因；成功 `{ok:true, reason:None}`）、`load_dismissed` / `save_dismissed`（`preferences.json` 结束标记）；`RuntimeConfig::load_lenient`（`src/config.rs`，与 `load` 共用 `load_inner`，允许缺 key 启动供桌面侧车首启）。验证 `cargo test -p yi-agent-runtime --lib onboarding` 与 `cargo test -p yi-agent-runtime --lib config` — [设计](../superpowers/specs/2026-10-09-first-run-onboarding-design.md)。**注**：原 `write_model_settings`（把四个模型键写进 `.env`）已随「模型配置单一真相源」删除——模型键不再有写 `.env` 的路径。

- [x] 清单运行期新鲜度：严格读取 `try_load_catalog_from` + 指纹自更新 `CatalogWatcher` — `try_load_catalog_from`（`src/models.rs:152`）区分「文件不存在」= `Ok(空清单)` 与「不可读/不可解析」= `Err(reason)`，与宽松 `load_catalog_from`（绝不报错）**互补**，二者共用抽出的 `parse_catalog`（日志字段含 `path`，口径逐字节一致）；`CatalogWatcher`（`src/models.rs:203`）= `new(path)`（构造即读一次，记 `(mtime, size)` 指纹）/ `from_catalog(path, catalog)`（固定快照，`pinned` 标志，永不重读）/ `current() -> Arc<ModelCatalog>`（指纹未变直接返回缓存 `Arc`，零解析；指纹变化且解析成功→换缓存并推进指纹；**解析失败→保留 last-known-good 且不推进指纹**，故文件修好后下一次调用立即生效；文件不存在→采纳空清单）。`EffectiveKey` 刻意不派生 `Debug`（`api_key` 敏感）。验证：`cargo test -p yi-agent-runtime --lib`（136，含 `try_load_distinguishes_absent_from_corrupt` 与 `watcher_reuses_the_cached_catalog_when_the_fingerprint_is_unchanged` / `watcher_picks_up_a_changed_file` / `watcher_keeps_last_known_good_across_a_corrupt_file_and_heals` / `watcher_adopts_an_absent_file_as_empty` / `from_catalog_is_a_fixed_snapshot`）— [设计](../superpowers/specs/2026-10-17-model-catalog-live-reload-design.md)

- [x] 模型配置单一真相源：`.env` 退出模型领域 — `MODEL_ENV_KEYS`（`src/config.rs:13`，9 个模型键）声明「不再从 `.env` 读」的键集；`ModelEnvSnapshot::take()` 在加载任何 `.env` **之前**快照模型键，`prune_dotenv_model_keys` 在加载后把「因 dotenvy 而来」的键从进程 env 剔除（真实 shell 导出不动）；`load_inner`（`src/config.rs:241`）只读唯一的全局 `.env`（`resolve_global_env_path`，`:185`），`resolve_env_path` / `is_workdir_explicit` 及项目级 `.env` 概念一并删除；共享导入器 `onboarding::import_settings_into_catalog`（`src/onboarding.rs:261`，重名加 `-2` 后缀、设为默认、空 `api_url` 回退 provider 默认）供 CLI 引导与 `onboarding/apply` 共用。验证：`cargo test -p yi-agent-runtime --lib`（164，含 `a_dotenv_model_key_never_reaches_the_config` / `prune_leaves_a_real_process_env_key_alone` / `importing_settings_suffixes_on_a_name_collision`）— [设计](../superpowers/specs/2026-10-17-single-source-of-truth-model-config-design.md)
- [x] 存量 `.env` → 清单一次性迁移 + 三入口接线 — `migrate::migrate_model_keys`（`src/migrate.rs:81`）**先落清单、后注释 `.env`**（顺序反向会造出「键注释了、清单没落」的丢失），严格读取 `try_load_catalog_from` 判「清单为空」（损坏文件绝不被静默覆盖），`api_key`/`api_url` 缺 `MODEL_*` 时回退 provider 命名键（`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `*_BASE_URL`）；`log_outcome`（`src/migrate.rs:227`）区分五种结果。三入口接线：CLI `run_agent`（`crates/yi-agent/src/main.rs`）、`yi-agent web`、app-server 两个生产入口（`crates/yi-agent-app-server/src/server.rs:1864` 的 `migrate_legacy_model_keys_on_startup`，`run` 与 `serve_stdio_with_relay` 各调一次；测试构造器不调）。验证：`cargo test -p yi-agent-runtime --lib migrate::`（17）+ `cargo test -p yi-agent-app-server --lib startup_migrates_legacy_model_keys_into_the_catalog` — [设计](../superpowers/specs/2026-10-17-single-source-of-truth-model-config-design.md)
- [x] 清单读取/写入不得把「读不出来的既有 `models.json`」当成「没有配置」（静默覆盖修复）— 与 `try_load_catalog_from` 同源的**第二类**静默丢失：`parse_catalog`（`src/models.rs:275`）此前对**合法 JSON 但顶层不是对象**（数组 / 字符串 / 数字 / `null` / 布尔）以及**顶层是对象、但 `models[]` 每条都被逐条过滤跳过**（如唯一一条漏了 `api_url`）两种输入都返回 `Ok(空清单)`；`migrate_model_keys`（`src/migrate.rs:111`）用「清单为空」当「可安全导入」的闸门，于是 `save_catalog_to` 的 rename 直接盖掉用户整份文件（与已修的 F1「损坏 JSON」同类，只是入口不同）。同时 `save_catalog_to`（`src/models.rs:312`）自己也有一份**独立的**宽容读（`read→from_str→as_object→unwrap_or_default`），对损坏 / 非对象 base 一律从空对象起步——`model/add`、`model/delete` 与引导导入器都能经这条路抹掉用户的清单。修复：(1) `parse_catalog` 顶层非对象 → `Err`（不再是 `Ok(空)`）；`models` 非空数组却幸存 0 条 → `Err`（「写了但全写坏了」≠「没配置」）；(2) `save_catalog_to` 的 base 读不出来 → `Err(InvalidData)` 且**保持文件原样**，只有文件**不存在**才从空对象起步（真正的首写）。`load_catalog_from` 仍是「Err → 空清单」（坏文件不阻断启动），故容错语义不变、无调用方回归。验证：`cargo test -p yi-agent-runtime --lib`（165，含 `try_load_rejects_a_document_that_is_not_a_json_object`、`try_load_rejects_a_catalog_whose_entries_were_all_skipped`、`saving_refuses_to_overwrite_a_base_it_cannot_read`、`migrate::tests::a_non_object_catalog_is_never_clobbered`、`migrate::tests::an_all_entries_skipped_catalog_is_never_clobbered`；把 `parse_catalog` 的顶层守卫还原后 4 条转红、`save_catalog_to` 守卫还原后 `saving_refuses_*` 转红，故确认有鉴别力）+ `cargo test -p yi-agent-app-server --lib`（826，含 `model/add` 全路径）+ `cargo test -p yi-agent`（649）— 与设计 `2026-10-17-single-source-of-truth-model-config-design.md` §5「清单不可写 → `.env` 不被注释」同一不变量

- [x] 模型级 reasoning effort 能力：落盘结构 + 读写容错 + 运行期与写入期约束 — `ReasoningCapabilities`（`src/models.rs:105`，三通道 `chat`/`responses`/`anthropic`，各 `Option<ChannelCapability>`）与 `ChannelCapability`（`:91`，`supported: Vec<String>` + `probed_at: u64` + `error: Option<String>`），挂在 `ModelEntry.reasoning_capabilities`（`:79`）；读路径容错（`#[serde(default)]` + 自定义反序列化：非对象 → `None`、未知档位丢弃、坏 `probed_at` → 0）。**三个同源判定**：`effective_channel`（`:149`，`auto` 优先 responses，只有 chat 可信而 responses 不可信才落 chat）、`supported_efforts`（`:173`，有 `error` → `None` = 能力未知）、`effort_is_allowed`（`:186`，未知 → 放行）。`resolve_effective`（`:610` 一带）在赋值后过滤不支持档位（置 `None` + `tracing::info`），因为发出去必然被拒、只会让 provider 白跑一轮剥离重投。**探测编排** `src/reasoning_probe.rs`：`probe_entry`（`:54`，按 `api_style` 选通道；`auto` 走 `detect_openai_channel` 只测判定出的那条）、基线（不带 effort）+ 7 档串行、基线失败即 `supported: []` + `error` 不再逐档、`PROBE_REQUEST_TIMEOUT = 10s`（`:13`）/ `PROBE_TOTAL_BUDGET = 30s`（`:15`）超预算保留已测结果。验证：`cargo test -p yi-agent-runtime --lib`（179，含 `reasoning_capabilities_roundtrip_and_drop_unknown_tiers` 与 `reasoning_probe::tests`）— [设计](../superpowers/specs/2026-10-19-reasoning-effort-capability-probe-design.md) §4/§5/§6

**验证命令：** `cargo test -p yi-agent-runtime`（179 个测试）
