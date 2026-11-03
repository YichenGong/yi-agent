# yi-agent-plugins

## 模块说明

插件**契约核心**：值类型、发布清单解析与错误分类。它是 T06 引入的独立 crate，
为后续信任验证（T07）、有界解包（T08）、注册表（T09）提供共同底座。

设计见 [2026-11-02-kanban-independent-release-design.md](../superpowers/specs/2026-11-02-kanban-independent-release-design.md)；
契约见 [G1](../superpowers/specs/2026-11-02-plugin-host-protocol.md)、[G2](../superpowers/specs/2026-11-02-plugin-distribution-lifecycle.md)、[G3](../superpowers/specs/2026-11-02-kanban-plugin-migration.md)。

## 范围边界

**做什么：**
- `types.rs`：`PluginId`、`ProjectId`、`ReleaseId`、`Binding`、`Effect`、`Operation`
- `manifest.rs`：发布清单的强类型解析 + 语义校验
- `trust.rs`：签名 payload 编码、验签、有效期/序号/撤销、信任根轮换
- `error.rs`：`PluginError` 的 16 个分类与稳定 `kind` 字符串

**不做什么：**
- 不读写实例状态、不启动进程、不依赖任何运行时（路径与时钟由调用方注入）
- 不依赖 store / app-server / Tauri / 看板，也不依赖 tokio
- 解包、注册表、生命周期分别在 T08–T12

## Features

- [x] 值与错误类型 — `src/types.rs`、`src/error.rs`。`PluginError` 有 16 个变体，`kind()` 给出线协议字符串。判据：`cargo test -p yi-agent-plugins --test manifest`
- [x] 清单强类型解析 — `src/manifest.rs` 的 `Manifest::parse`；`deny_unknown_fields` 让宿主不认识的必需字段直接拒绝。判据：`an_unknown_required_field_is_rejected`
- [x] 包内路径安全 — `validate_package_path` 拒绝绝对路径、任何 `..` 段、反斜杠与 NUL。判据：`absolute_ui_entry_is_rejected`、`a_parent_directory_entry_is_rejected`、`a_backslash_entry_is_rejected`
- [x] 标识符只能校验构造 — `PluginId` / `ProjectId` 手写 `Deserialize` 走 `parse`，不用 `transparent` 直构。判据：`an_illegal_plugin_id_is_rejected`
- [x] 贡献名唯一 — views / skills / methods / cli 四类各自查重。判据：四条 `a_duplicate_*_is_rejected`
- [x] 恰好一个 main 视图 — 判据：`a_second_main_view_is_rejected`、`a_manifest_without_a_main_view_is_rejected`
- [x] `Maintenance` 不可由插件声明 — methods、cli、capabilities 三处都拒绝。判据：`maintenance_may_not_be_declared_by_a_plugin`
- [x] Read 不得命名破坏性动词 — `DESTRUCTIVE_VERBS` 与 `scripts/plugins/check-contracts.mjs` 保持同表。判据：`a_manifest_declaring_read_with_a_destructive_method_is_rejected`
- [x] 迁移逐版本相邻 — `to == from + 1`，否则恢复无法判断从哪份备份重做。判据：`a_skipped_migration_version_is_rejected`、`an_adjacent_migration_chain_is_accepted`
- [x] 依赖边界干净 — 只有 serde / serde_json / thiserror（dev 加 tempfile）。判据：`cargo tree -p yi-agent-plugins` 不含 sqlite / Tauri / app-server / tokio / 看板
- [x] 签名 payload 编码与参考实现逐字节一致 — `src/trust.rs` 的 `encode_root_payload` / `encode_release_payload`。判据：`the_rust_payload_encoder_is_byte_identical_to_the_reference_implementation`（与 fixture 的 `payloadB64` 比对）
- [x] 验签密钥来自信任集，不来自被验证文档 — `TrustStore::verify_release` 要求 `keyId` 等于已信任根。判据：`an_unknown_signing_key_is_rejected`
- [x] 四条时间/序号拒绝 — 过期、未来、回放、未知 key。判据：`an_expired_release_is_rejected_even_with_a_valid_signature`、`a_release_published_in_the_future_is_rejected`、`a_serial_below_the_accepted_one_is_rejected_as_a_replay`、`an_unknown_signing_key_is_rejected`
- [x] 只有签名能拦的篡改测试 — 改字段并重算 `payloadB64`。判据：`rewriting_a_field_and_recomputing_the_payload_still_fails_the_signature`（变异「验签恒真」打红 1 条）
- [x] 信任根不能自签 — 只接受编译时 pinned 的 keyId。判据：`a_root_envelope_cannot_bootstrap_itself`、`the_pinned_root_set_is_explicit_and_small`
- [x] 换根必须由当前根授权且序号递增 — 判据：`a_root_rotation_must_be_authorized_by_the_existing_root`、`a_root_rotation_authorized_by_a_different_root_is_rejected`、`a_root_rotation_with_a_lower_serial_is_rejected`、`a_root_rotation_authorized_by_the_current_root_is_accepted`
- [x] 失败不污染 trust store — 只在整体验证成功后推进接受序号。判据：`a_rejected_release_does_not_raise_the_floor_for_a_lower_serial`、`a_failed_verification_does_not_advance_the_accepted_serial`
- [x] `VerifiedRelease` 不可伪造 — 无公开构造函数、不实现 `Deserialize`。判据：`a_verified_release_cannot_be_built_from_unverified_json`
- [x] 错误不泄漏密钥/签名 — 判据：`errors_do_not_leak_key_material_or_raw_signatures`
- [x] 正式源码不含测试种子 — 判据：`the_shipped_sources_do_not_contain_a_test_seed_or_a_private_key`
- [ ] 有界解包 — T08
- [ ] 实例注册表 — T09
- [ ] 有界解包 — T08
- [ ] 实例注册表 — T09
