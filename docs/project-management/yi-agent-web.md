# yi-agent-web

## 模块说明

yi-agent 的 Web 配置管理界面。通过 axum Web 服务器提供内嵌 HTML 单页应用，管理 15 个环境变量的配置，支持全局 + 本地配置文件层级合并。

## 范围边界

**做什么：**
- `yi-agent web` 子命令启动 Web 服务器（默认 127.0.0.1:7292）
- 15 个环境变量的元数据管理（分组、类型、默认值、选项）
- 配置文件读写（`.yi-agent/.env`）
- 全局 + 本地配置层级合并（本地覆盖全局）
- 垂直标签页 + 可折叠分区 UI
- Secret 值掩码显示

**不做什么：**
- 不做用户认证（本地运行，YAGNI）
- 不做多用户支持
- 不做配置版本历史
- 不做配置导入/导出

## Features

- [x] WebUI 配置服务器 — `crates/yi-agent-web/src/lib.rs::serve()` 启动 axum — [设计](../plans/2026-07-24-web-config-ui-design.md)
- [x] 15 个环境变量元数据管理 — `config_meta.rs` 定义全部 env var 元数据 — [设计](../plans/2026-07-25-web-config-ui-restructure-design.md)
- [x] 配置文件层级合并 — `env_file.rs` + `config.rs::load_env_files()` 实现本地覆盖全局 — [设计](../plans/2026-07-25-config-layering-design.md)
- [x] 垂直标签页 + 可折叠分区 UI — `assets/` 内嵌 HTML 实现 — [设计](../plans/2026-07-25-web-config-ui-restructure-design.md)
- [x] Secret 值掩码与安全写入 — `env_file.rs::mask()` 对读取值脱敏，`api.rs::put_config()` 跳过未修改的掩码值；验证：`cargo test -p yi-agent-web --test api_test get_config_masks_secret_values`
- [x] 全局配置优先打开 — `assets/index.html` 将全局置于本地左侧并在全局路径缺失时回退本地；验证：`cargo test -p yi-agent-web --test api_test index_html_defaults_to_global_scope_before_local_scope`
