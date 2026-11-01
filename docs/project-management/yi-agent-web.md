# yi-agent-web

## 模块说明

yi-agent 的 Web 配置管理界面。通过 axum Web 服务器提供内嵌 HTML 单页应用，编辑
`<instance_home>/settings.json`（运行参数：轮次、压缩、沙箱等）。字段元数据来自
`yi-agent-runtime` 的 `settings.rs`，与桌面端「设置 → 运行参数」同源同文件。

## 范围边界

**做什么：**
- `yi-agent web` 子命令启动 Web 服务器（默认 127.0.0.1:7292）
- `<instance_home>/settings.json` 的读写（`GET /api/config`、`PUT /api/config`）
- 字段元数据（分组、类型、默认值、选项）由 runtime 单点提供，Web 侧不另抄一份
- 垂直标签页 + 可折叠分区 UI（分组由 API 数据派生，不硬编码）

**不做什么：**
- 不做用户认证（本地运行，YAGNI）
- 不做多用户支持
- 不做配置版本历史
- 不做配置导入/导出
- 不管理模型配置（模型在 `models.json`，由桌面端「设置 → 模型」或 `model/*` RPC 管理）
- 不再读取或写入 `.env`（该文件已从产品中移除）

## Features

- [x] WebUI 配置服务器 — `crates/yi-agent-web/src/lib.rs::serve()` 启动 axum — [设计](../plans/2026-07-24-web-config-ui-design.md)
- [x] Web UI 编辑 `<instance_home>/settings.json`（元数据来自 runtime 的 `settings.rs`）— `api.rs::get_config` / `api.rs::put_config`，`router(settings_path)` 由调用方注入路径，便于测试隔离
  - 判据：`cargo test -p yi-agent-web`
- [x] 保存配置不丢键：未知顶层键原样保留，未设置的字段不被物化（不写 `null`）— `api.rs::put_config` 走 `settings::load_from` → 改字段 → `write_to`
  - 判据：`cargo test -p yi-agent-web --test api_test put_config_preserves_unknown_keys`
  - 判据：`cargo test -p yi-agent-web --test api_test put_config_does_not_materialize_unset_fields`
- [x] 单字段非法值返回 400 且整份不落盘，避免把别的字段一起写坏
  - 判据：`cargo test -p yi-agent-web --test api_test put_config_rejects_an_invalid_value_with_400`
- [x] 读取区分「未设置」与「设为默认值」：`value` 为 `null` 时同时给出 `default`，前端才画得出「未设置 · 默认 X」
  - 判据：`cargo test -p yi-agent-web --test api_test get_config_reports_null_for_unset_and_default_source`
- [x] 删除「真实 LLM 测试」面板（职能由「设置 → 模型」+ `model/probe` 覆盖）— `assets/index.html` 移除该面板，相关路由一并删除
  - 判据：`cargo test -p yi-agent-web --test api_test the_real_llm_test_routes_are_gone`
- [x] 垂直标签页 + 可折叠分区 UI — `assets/` 内嵌 HTML 实现 — [设计](../plans/2026-07-25-web-config-ui-restructure-design.md)
- [x] 字段带 `label`（中文显示名）— `api.rs::get_config` 输出 `spec.label`，与桌面端 `settings_rpc.rs` 同源
  - 判据：`cargo test -p yi-agent-web --test api_test get_config_reports_a_label_for_every_field`
- [x] `value: null` 表示删键，支持前端「恢复默认」— `api.rs::UpdateItem::value` 为 `Option<String>`，`None` 转 `Value::Null`
  - 判据：`cargo test -p yi-agent-web --test api_test put_config_with_null_value_deletes_the_key`
  - 判据：`cargo test -p yi-agent-web --test api_test put_config_treats_a_missing_value_as_null`
- [x] 前端分组从 `data.groups` 派生，不再硬编码 `['Agent','Tools']` — `assets/index.html` 的 `groupNames` + `GROUP_LABELS`
  - 判据：`cargo test -p yi-agent-web --test api_test get_config_groups_match_every_group_in_fields`
  - 判据：`cargo test -p yi-agent-web --test api_test index_html_derives_groups_instead_of_hardcoding_them`
- [x] 页面显示中文标签、数字 `min`/`max`、select 空选项与「恢复默认」按钮 — `assets/index.html::renderField`
  - 判据：`cargo test -p yi-agent-web --test api_test index_html_derives_groups_instead_of_hardcoding_them`（弱契约，真正渲染靠人工验收）

**验证命令：** `cargo test -p yi-agent-web`（10 个测试，全部在 `tests/api_test.rs`）
