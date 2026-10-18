# yi-agent-web

## 模块说明

yi-agent 的 Web 配置管理界面。通过 axum Web 服务器提供内嵌 HTML 单页应用，管理 7 个非模型环境变量的配置（模型键已迁到 `~/.yi-agent/models.json`），单一全局配置文件。

## 范围边界

**做什么：**
- `yi-agent web` 子命令启动 Web 服务器（默认 127.0.0.1:7292）
- 7 个非模型环境变量的元数据管理（分组、类型、默认值、选项）
- 单一全局配置文件读写（`~/.yi-agent/.env`，无项目级 `.env`）
- 垂直标签页 + 可折叠分区 UI
- Secret 值掩码显示

**不做什么：**
- 不做用户认证（本地运行，YAGNI）
- 不做多用户支持
- 不做配置版本历史
- 不做配置导入/导出

## Features

- [x] WebUI 配置服务器 — `crates/yi-agent-web/src/lib.rs::serve()` 启动 axum — [设计](../plans/2026-07-24-web-config-ui-design.md)
- [x] 7 个非模型环境变量元数据管理 — `config_meta.rs:26` 的 `ALL_VARS`（16 → 7 条；9 个模型键移除）；验证：`cargo test -p yi-agent-web --lib config_meta`
- [x] 单一全局配置文件 — `api.rs::AppState` / `lib.rs::serve()` 从「本地 + 全局」双路径收敛为单一 `env_path`，`scope` 字段失效即忽略；验证：`cargo test -p yi-agent-web --test api_test put_config_writes_the_single_file`
- [x] 垂直标签页 + 可折叠分区 UI — `assets/` 内嵌 HTML 实现 — [设计](../plans/2026-07-25-web-config-ui-restructure-design.md)
- [x] Secret 值掩码与安全写入 — `env_file.rs::mask()` 对读取值脱敏，`api.rs::put_config()` 跳过未修改的掩码值；验证：`cargo test -p yi-agent-web --test api_test get_config_masks_secret_values`
- [x] 模型键移出 env 编辑器（不再有绕过清单的写入口）— `config_meta::ALL_VARS` 删掉 9 个模型键（分组 3 → 2），`assets/index.html` 移除 Model Provider 标签页并加一行「模型请在桌面端设置」提示，`api.rs::get_config` 过滤空分组；验证：`cargo test -p yi-agent-web --lib config_meta::tests::model_keys_are_no_longer_editable` — [设计](../superpowers/specs/2026-10-17-single-source-of-truth-model-config-design.md)

**验证命令：** `cargo test -p yi-agent-web`（37 个测试：lib 25 + api_test 12）
