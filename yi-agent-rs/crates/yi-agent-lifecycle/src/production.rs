//! 共享的非业务管理 DTO：项目准备、逐 root 授权索引与授权快照。
//!
//! 这一层刻意只放**线上类型**，不放任何 I/O：桌面壳、app-server 与 supervisor
//! 三方都要按同一套字段讲话，而壳只允许依赖本 crate（不能拉进 SQLite/supervisor）。
//! 因此这里的每个类型都必须能只靠 `serde` 序列化，且秘密只以
//! [`SecretCapability`] 的形式出现——它的 `Debug` 打印 `<redacted>`。
//!
//! 三条约束写进类型里，而不是留给调用方记得：
//!
//! - [`PrepareOutcome`] 是判别结果，不是 `Result<(), _>`：`starting`、`blocked` 与
//!   `unknown` 各有不同含义，压成一个 `Err` 会让前端把"还在准备"当成"失败"。
//! - [`RootAuthorityKey`] 用 `root_id`（daemon 的 root 身份）而不是目录路径：同一
//!   目录下的两个会话是两个 root，路径相等不能推出授权可以互换。
//! - [`RootAuthoritySnapshot`] 带 `sidecar_epoch` 与每 scope 单调的 `revision`：旧
//!   revision 必须被拒绝，空快照表示撤销该 scope 的全部授权。

use crate::foreground::{RootBindIntent, SecretCapability};
use crate::types::{ArtifactIdentity, RuntimeIdentity, RuntimeScope};
use serde::{Deserialize, Serialize};

/// 一次项目准备请求。
///
/// `attempt_id` 由发起方生成，用于把迟到结果与当前这次准备对上：准备是异步的，
/// 用户切换项目后旧结果不得落进新项目。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub scope: RuntimeScope,
    pub expected: ArtifactIdentity,
    pub attempt_id: String,
}

/// 一次项目准备的判别结果。
///
/// `Connected` 只表示 runtime 身份与期望匹配，**不**表示壳已持有租约，更不表示任何
/// 会话获得 root 授权：那两步是 `starting` 之后各自的成功条件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrepareOutcome {
    /// 已连接可用的受管 runtime。
    Connected(RuntimeIdentity),
    /// 已连接，但构建不同且控制协议兼容：需要用户确认版本切换。
    VersionChoiceRequired(RuntimeIdentity),
    /// 观测到 unmanaged runtime：只能是用户显式选择暂用或走迁移。
    LegacyChoiceRequired { reason: String },
    /// 条件不满足，且**不得**降级为 embedded。
    Blocked { reason: String },
    /// 无法核实或应答丢失：只读对账，不推断成功或失败。
    Unknown { reason: String },
}

/// 一条 root 授权的精确索引。
///
/// 五段都必须匹配才命中：`scope_key` 说哪个项目，`generation` 说哪一代 daemon，
/// `client_run_id` 说哪个客户端，`thread_key` 说哪个会话，`root_id` 说 daemon 的哪个
/// root。少了任何一段，换项目、换代或换会话都会误命中。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RootAuthorityKey {
    pub scope_key: String,
    pub generation: String,
    pub client_run_id: String,
    pub thread_key: String,
    pub root_id: String,
}

/// 一条 root 授权：索引加 capability。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootAuthorityEntry {
    pub key: RootAuthorityKey,
    /// 交给 daemon 验证 attach 的秘密。`Debug` 只打印 `<redacted>`。
    pub capability: SecretCapability,
}

/// 某一 scope 的授权快照。
///
/// `sidecar_epoch` 必须等于认证 bootstrap 固定下来的 epoch，不从快照自身切换；
/// `revision` 按 scope 单调，旧值被拒绝；`entries` 为空即撤销该 scope 的全部授权。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootAuthoritySnapshot {
    pub sidecar_epoch: String,
    pub scope_key: String,
    pub revision: u64,
    pub entries: Vec<RootAuthorityEntry>,
}

/// 一个会话请求 root 授权的意图。
///
/// `Deadline` 不序列化：接收方钳制到自己配置的剩余预算，不信任对端延长窗口。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootIntentRequest {
    pub sidecar_epoch: String,
    pub scope_key: String,
    pub client_run_id: String,
    pub intent: RootBindIntent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key(root_id: &str) -> RootAuthorityKey {
        RootAuthorityKey {
            scope_key: "/tmp/runtime".into(),
            generation: "guardian-1".into(),
            client_run_id: "run-1".into(),
            thread_key: "thread:abc".into(),
            root_id: root_id.into(),
        }
    }

    fn entry(root_id: &str, secret: &str) -> RootAuthorityEntry {
        RootAuthorityEntry {
            key: key(root_id),
            capability: SecretCapability::new(secret),
        }
    }

    /// 序列化后能读回来，且不再持有原秘密对象。
    #[test]
    fn a_root_authority_entry_round_trips() {
        let original = entry("root-1", "cap-secret-value");
        let json = serde_json::to_string(&original).unwrap();
        let decoded: RootAuthorityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.key, original.key);
        assert_eq!(decoded.capability.expose(), "cap-secret-value");
    }

    /// `Debug` 是这层唯一允许打印 DTO 的地方，秘密绝不能出现在里面。
    #[test]
    fn a_root_authority_entry_never_prints_its_secret() {
        let entry = entry("root-1", "cap-super-secret-value");
        let printed = format!("{entry:?}");
        assert!(
            !printed.contains("cap-super-secret-value"),
            "the secret leaked into Debug: {printed}"
        );
        assert!(printed.contains("<redacted>"), "{printed}");
    }

    /// 快照本身也不能泄露秘密。
    #[test]
    fn a_snapshot_never_prints_any_secret() {
        let snapshot = RootAuthoritySnapshot {
            sidecar_epoch: "epoch-1".into(),
            scope_key: "/tmp/runtime".into(),
            revision: 7,
            entries: vec![
                entry("root-1", "first-secret"),
                entry("root-2", "second-secret"),
            ],
        };
        let printed = format!("{snapshot:?}");
        assert!(!printed.contains("first-secret"), "{printed}");
        assert!(!printed.contains("second-secret"), "{printed}");
    }

    /// 索引是精确匹配的键：root id 不同即不同授权，路径拼写不参与。
    #[test]
    fn root_ids_make_distinct_keys() {
        assert_ne!(key("root-1"), key("root-2"));
        let mut same = key("root-1");
        same.root_id = "root-2".into();
        assert_ne!(same, key("root-1"));
    }

    /// 准备请求/结果可往返，且 `Deadline` 不在任何 DTO 里。
    #[test]
    fn prepare_dtos_round_trip_and_carry_no_deadline() {
        let request = PrepareRequest {
            scope: RuntimeScope {
                key: "/tmp/runtime".into(),
                runtime_dir: PathBuf::from("/tmp/runtime"),
                project_key: "/tmp/project".into(),
                instance_home: PathBuf::from("/tmp/home"),
            },
            expected: ArtifactIdentity {
                build_id: "build-1".into(),
                display_version: "0.1.13".into(),
                executable: PathBuf::from("/tmp/yi-agent"),
                sha256: String::new(),
                schema_min: 0,
                schema_max: 15,
                handover_version: 1,
                business_protocol_min: 2,
                business_protocol_max: 2,
                capabilities: vec!["control.v1".into()],
            },
            attempt_id: "attempt-1".into(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("deadline"), "{json}");
        assert_eq!(
            serde_json::from_str::<PrepareRequest>(&json).unwrap(),
            request
        );

        for outcome in [
            PrepareOutcome::LegacyChoiceRequired {
                reason: "unmanaged".into(),
            },
            PrepareOutcome::Blocked {
                reason: "schema".into(),
            },
            PrepareOutcome::Unknown {
                reason: "no answer".into(),
            },
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            assert_eq!(
                serde_json::from_str::<PrepareOutcome>(&json).unwrap(),
                outcome
            );
        }
    }

    /// 意图请求同样可以往返，且保留 `previous_root_capability` 的"仅恢复"语义。
    #[test]
    fn a_root_intent_request_round_trips() {
        let request = RootIntentRequest {
            sidecar_epoch: "epoch-1".into(),
            scope_key: "/tmp/runtime".into(),
            client_run_id: "run-1".into(),
            intent: RootBindIntent {
                operation_id: "op-1".into(),
                thread_key: "thread:abc".into(),
                workspace: PathBuf::from("/tmp/project"),
                extra_roots: vec![PathBuf::from("/tmp/project/extra")],
                previous_root_capability: Some(SecretCapability::new("old-cap")),
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("deadline"), "{json}");
        let decoded: RootIntentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            decoded
                .intent
                .previous_root_capability
                .as_ref()
                .map(SecretCapability::expose),
            Some("old-cap")
        );
    }
}
