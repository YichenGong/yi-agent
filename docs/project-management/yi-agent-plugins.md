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
- [x] 有界解包上限与硬顶 — `src/limits.rs` 的 `ExtractLimits`：归档 256 MiB、总量 1 GiB、单文件 128 MiB、20 000 个文件；`exceeds_hard_ceiling()` 拒绝插件抬高上限。判据：`limits_cannot_be_raised_by_the_plugin`（用抬高后的上限安装必须 `PermissionDenied` 且不落盘）
- [x] 解包输入是真实验证产物 — `install_verified` 接 `&VerifiedRelease` + `&Manifest`，而不是"摘要 + 字节"，调用方无法拼一个未验证的包。判据：`a_payload_that_does_not_match_the_manifest_is_caught_by_the_signed_digest`
- [x] 八类归档攻击被拒 — 路径穿越、绝对路径、反斜杠、symlink、hardlink、设备文件、重复名、大小写碰撞。判据：`a_parent_directory_entry_is_rejected` 等八条；test fixture 手写 tar header，因为 `tar::Builder` 会替我们拒绝 `..`，用它会造不出攻击样本
- [x] 解包分两遍且边解边计 — 第一遍只读条目头累计计数，第二遍才写；`walk_archive`（`src/bundle.rs:404`）。判据：`exceeding_the_total_size_limit_is_rejected` 等四条上限用例，每条都先证明默认上限下同一个包能装
- [x] 失败不留残骸 — staging 与 destination 同级、失败整目录删除、摘要文件在 staging 内生成后整体 `rename`。判据：`a_rejected_archive_leaves_no_destination_directory`（同时断言 `.installed.staging` 不存在）
- [x] 归档权限不采纳 — 普通文件 0444，只有清单 `program.entry` 设 0555；setuid 与原始 mode 丢弃。判据：`setuid_is_not_preserved`、`a_non_entry_file_is_not_executable`、`a_declared_program_entry_becomes_executable`
- [x] 声明入口必须存在 — 清单声明了归档里没有的入口，在解包阶段就拒，而不是等宿启进程时才发现。判据：`a_declared_program_entry_missing_from_the_archive_is_rejected`
- [x] 安装后完整性 — `.yi-agent-digests` 记录每文件摘要，`verify_installed` 启动前比对。判据：`an_unmodified_installation_passes_the_integrity_check`、`a_tampered_installed_file_fails_the_integrity_check`
- [x] 实例注册表与原子记录 — `src/registry.rs` 的 `Registry::open/record_install/select_version/list`；`schemaVersion` 不符 → `Incompatible`，JSON 损坏 → `RecoveryRequired`（不把损坏当空表，否则一次安装抹掉全部记录）。判据：`a_corrupt_registry_is_reported_and_left_untouched`、`a_registry_corrupted_after_open_is_not_clobbered_by_a_write`、`a_registry_with_an_unknown_schema_version_is_rejected`
- [x] 安装幂等与冲突 — 同 `(plugin, version, sha256)` 幂等且不重写文件；同版本不同摘要 → `Conflict` 且不覆盖。判据：`reinstalling_the_same_digest_is_idempotent`（比对文件字节）、`the_same_version_with_a_different_digest_is_a_conflict`
- [x] 安装不产生运行授权 — 首次安装只设选中版本，`grants` 为空；第二个版本装进来不改变选中版本。判据：`the_first_install_selects_the_version_without_creating_a_grant`、`a_second_version_does_not_change_the_selected_one`
- [x] 跨进程互斥 — `src/locks.rs` 用 `flock`；进程内按路径去重（`flock` 按打开文件描述加锁，同进程两次 open 会各拿一把）。判据：`a_lifecycle_lock_is_exclusive_across_processes`、`two_processes_do_not_both_hold_the_lock`、`two_acquires_in_one_process_yield_the_same_lock`
- [x] registry 短锁防丢失更新 — 两个进程并发记录不同插件，两条都必须存活。判据：`two_processes_recording_different_plugins_do_not_lose_an_update`（删掉短锁即打红）
- [x] 读者看不到半个文件 — 唯一临时文件 + 文件 fsync + rename + 目录 fsync。判据：`readers_never_observe_a_partial_file`（并发读线程，一次解析失败即红）
- [x] 不同实例互不干扰 — 判据：`different_instance_homes_do_not_share_locks`
- [x] 项目规范化 — `src/project.rs` 的 `ProjectResolver`：`canonicalize` 跟随 symlink，按路径**分量**而非字符串前缀判子目录。判据：`a_symlink_resolves_to_the_same_project_as_its_target`、`a_sibling_directory_sharing_a_prefix_is_not_a_subdirectory`、`same_named_directories_are_different_projects`、`a_nested_registered_root_wins_over_its_parent`
- [x] 项目授权代次独立 — 撤销或变更只递增该项目代次，其他项目不受影响；旧代次绑定 → `StaleBinding`。判据：`revoking_one_project_leaves_the_other_usable`、`a_stale_grant_generation_is_rejected`、`a_real_change_does_bump_the_generation`
- [x] 授权绑定宿主身份 — 别的宿主不能复用。判据：`a_binding_from_another_host_is_rejected`
- [x] 无变更的重复启用幂等 — 不递增代次，否则客户端刚拿到的绑定会无故失效。判据：`regranting_the_same_terms_is_idempotent_and_does_not_bump_the_generation`
- [x] 能力分类 — `Read`/`Write` 属基本授权；`Launch`/`QuotaWrite`/`Resident` 需独立确认（`is_independent`）。判据：`quota_and_resident_are_independent_confirmations`、`granting_basic_access_does_not_grant_residency`
- [x] 升级不自动扩权 — 新能力进 `pending` 并要求重新确认，旧能力不重置；`pending` 里的能力即便在 `granted` 中也拒绝。判据：`a_capability_increase_requires_reconfirmation`、`a_pending_capability_is_refused_even_if_it_is_listed_as_granted`、`a_pure_version_bump_does_not_require_reconfirmation`、`confirming_a_reconfirmation_clears_pending_and_bumps_the_generation`
- [x] 子目录不逃过根的撤销 — 判据：`a_revoked_grant_cannot_be_reused_through_a_subdirectory_path`
- [x] 授权持久化后重开仍拒旧绑定 — 判据：`a_grant_table_survives_a_round_trip_and_still_rejects_stale_bindings`
- [ ] 活动准入、排空屏障与幂等操作记录 — T11
