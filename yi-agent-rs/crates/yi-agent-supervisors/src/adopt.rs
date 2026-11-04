//! 接管编排：把一个旧安装移交给受管身份，**不丢数据、不并跑**。
//!
//! # 为什么接管不是一个"复制"动作
//!
//! 看板的受管状态目录与旧安装目录**是同一个路径**
//! （`<project>/.yi-agent/superpowers-kanban`，见 `main.rs::default_state_dir`
//! 与 `layout.rs`）。所以接管并不搬卡片；它要建立的是"谁有权推进这块看板"：
//!
//! 1. 旧进程真的停了（否则新旧同时推进同一队列）；
//! 2. 旧的数据被备份过（否则一次失败的接管没有退路）；
//! 3. 项目拿到了**新的**授权（旧开关不是授权，§8.1）；
//! 4. 登记是原子的（否则崩溃会留下半个受管身份）。
//!
//! # 冲突时报告，不杀进程
//!
//! §8.2 要求"旧 guardian 与新 owner 冲突时报告阻塞"。这里**不**按名字
//! `pkill`：同名进程可能属于另一个实例、另一个项目，甚至用户自己手动跑的
//! 一次调试。杀错一个进程的代价远高于"请用户自己关掉"。
//!
//! # 为什么所有外部动作都经过 trait
//!
//! "停旧服务"和"健康检查"需要真实进程与运行期控制面，而本 crate 刻意不依赖
//! 运行时。把它们放进 [`LegacyOwner`] 让测试能逐阶段失败，从而验证"任何一步
//! 失败都不留下半接管状态"——这正是 §8.2 的恢复要求。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use yi_agent_plugins::grants::GrantTable;
use yi_agent_plugins::lifecycle::Lifecycle;
use yi_agent_plugins::migration::Backup;
use yi_agent_plugins::ownership::ProjectOwnership;
use yi_agent_plugins::{Effect, LifecycleRequest, PluginError, PluginId, ProjectId, ReleaseId};

use crate::managed::{self, grants};

/// 接管时的外部动作。宿主实现它；核心只定义顺序与失败语义。
pub trait LegacyOwner {
    /// 旧服务现在是否还活着。
    ///
    /// **必须**基于真实存活判定（`flock` 试锁之类），不能用 PID 文件——
    /// PID 会被回收，一个死进程留下的 PID 会被当成活着的，接管就永远做不了。
    fn legacy_is_running(&self, project: &Path) -> Result<bool, PluginError>;

    /// 请旧服务退出并等到它真的退出。
    ///
    /// 只停**这个项目**的旧服务。实现方不得按进程名批量杀。
    fn stop_legacy(&self, project: &Path) -> Result<(), PluginError>;

    /// 接管后对项目做一次健康检查。
    fn health_check(&self, project: &Path, release: &ReleaseId) -> Result<(), PluginError>;

    /// 把项目投影到新代次（写绑定/常驻登记等）。
    fn project(
        &self,
        project: &Path,
        release: &ReleaseId,
        activation: u64,
    ) -> Result<(), PluginError>;
}

/// 一次接管的输入。
#[derive(Debug, Clone)]
pub struct AdoptRequest {
    /// 要接管的项目根（绝对路径，已规范化）。
    pub project: PathBuf,
    /// 旧数据目录（通常是 `<project>/.yi-agent/superpowers-kanban`）。
    pub legacy_state_dir: PathBuf,
    /// 用户为新授权勾选的能力。
    pub capabilities: Vec<Effect>,
    /// 是否把旧记录整体备份一次。默认 `true`；关掉只应该出现在测试里。
    pub backup: bool,
}

/// 接管成功后的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Adopted {
    pub project: String,
    pub transaction: String,
    pub activation: u64,
    /// 备份目录（未备份时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
}

/// 接管时发现的冲突。**每一个都要单独说清**，因为处理方式不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptError {
    /// 旧服务还在跑。
    LegacyStillRunning { project: PathBuf },
    /// 同一项目已经有活着的受管 owner。
    ManagedOwnerRunning { project: String, detail: String },
    /// 要迁移的数据不认得。
    UnrecognizedData { path: PathBuf, detail: String },
    /// 项目不在一个可接管的位置（不存在 / 不是目录）。
    ProjectUnavailable { path: PathBuf, detail: String },
    /// 用户没有勾选必需能力。
    MissingCapabilities { missing: Vec<Effect> },
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdoptError::LegacyStillRunning { project } => write!(
                f,
                "the legacy board is still running for {}; close it before adopting, because \
                 both would advance the same queue",
                project.display()
            ),
            AdoptError::ManagedOwnerRunning { project, detail } => write!(
                f,
                "project `{project}` already has a live managed owner ({detail}); adoption \
                 cannot preempt it"
            ),
            AdoptError::UnrecognizedData { path, detail } => write!(
                f,
                "the legacy data at {} is not in a format this release understands ({detail}); \
                 it will not be migrated in place",
                path.display()
            ),
            AdoptError::ProjectUnavailable { path, detail } => {
                write!(f, "project {} cannot be adopted: {detail}", path.display())
            }
            AdoptError::MissingCapabilities { missing } => write!(
                f,
                "the board cannot run without {}; confirm these before adopting",
                missing
                    .iter()
                    .map(|effect| format!("{effect:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for AdoptError {}

impl From<AdoptError> for PluginError {
    fn from(error: AdoptError) -> PluginError {
        match error {
            AdoptError::LegacyStillRunning { project } => PluginError::Busy(format!(
                "the legacy board is still running for {}",
                project.display()
            )),
            AdoptError::ManagedOwnerRunning { project, detail } => PluginError::Busy(format!(
                "project `{project}` already has a live managed owner ({detail})"
            )),
            AdoptError::UnrecognizedData { path, detail } => {
                PluginError::RecoveryRequired(format!("{}: {detail}", path.display()))
            }
            AdoptError::ProjectUnavailable { path, detail } => PluginError::NotFound(format!(
                "project {} cannot be adopted: {detail}",
                path.display()
            )),
            AdoptError::MissingCapabilities { missing } => PluginError::PermissionDenied(format!(
                "missing capabilities: {}",
                missing
                    .iter()
                    .map(|effect| format!("{effect:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// 备份目录名里的时间戳格式。秒级即可：同一秒内的第二次接管会被
/// `TargetNotEmpty`/`Busy` 挡住，不会撞名。
const BACKUP_SUFFIX: &str = "legacy-backup";

/// 接管一个旧项目。
///
/// 顺序（§8.2）：
///
/// ```text
/// 校验项目与数据 → 检查两个 owner 是否存活 → 备份 → 停旧服务
/// → 建立新的受管登记（授权 + 生命周期）→ 投影 → 健康检查
/// ```
///
/// 停旧服务放在备份**之后**：先备份再停，最坏情况是"旧进程多跑了一会儿"；
/// 反过来最坏情况是"旧进程停了但没备份"。前者可以重来，后者丢数据。
pub fn adopt(
    home: &Path,
    plugin: &PluginId,
    release: &ReleaseId,
    request: &AdoptRequest,
    owner: &dyn LegacyOwner,
    ownership: &ProjectOwnership,
    now: chrono::DateTime<chrono::Local>,
) -> Result<Adopted, PluginError> {
    // 1. 项目本身必须可用。不去猜一个相近路径。
    if !request.project.is_dir() {
        return Err(AdoptError::ProjectUnavailable {
            path: request.project.clone(),
            detail: "not a directory".to_string(),
        }
        .into());
    }
    let project_key = request.project.to_string_lossy().to_string();
    let project_id = ProjectId::parse(&project_key)?;

    // 2. 数据格式必须先认下来，再做任何写动作（§11：未知格式不原地猜）。
    let legacy = scan_legacy(&request.legacy_state_dir)?;

    // 3. 两个 owner 都不能活着。先查受管的——它是"已经有人接管过"的证据，
    //    比旧进程更值得先说。
    if ownership.is_alive(&project_key)? {
        let detail =
            "a live process holds this project's owner lock; close it before adopting".to_string();
        return Err(AdoptError::ManagedOwnerRunning {
            project: project_key,
            detail,
        }
        .into());
    }
    if owner.legacy_is_running(&request.project)? {
        return Err(AdoptError::LegacyStillRunning {
            project: request.project.clone(),
        }
        .into());
    }

    // 4. 新授权必须覆盖插件声明的必需能力。这里只报告缺什么，不替用户决定。
    let required = [Effect::Read, Effect::Write];
    let granted: BTreeSet<Effect> = request.capabilities.iter().copied().collect();
    let missing: Vec<Effect> = required
        .iter()
        .copied()
        .filter(|effect| !granted.contains(effect))
        .collect();
    if !missing.is_empty() {
        return Err(AdoptError::MissingCapabilities { missing }.into());
    }

    // 5. 备份。接管失败时这是唯一能退回旧记录的东西。
    let backup = if request.backup {
        let transaction_id = backup_transaction_id(now);
        let backup = Backup::new(home, transaction_id.clone());
        backup.take(
            &transaction_id,
            None,
            &legacy
                .files
                .iter()
                .map(|(relative, source)| (relative.clone(), source.clone()))
                .collect::<Vec<_>>(),
        )?;
        Some(backup.root().to_path_buf())
    } else {
        None
    };

    // 6. 停旧服务。到这里数据已经被安全保存，停不掉也不算丢东西。
    owner.stop_legacy(&request.project)?;
    // 停完必须复查：一个"答应退出"却没退的进程是最坏的情况——它会在
    // 认为接管成功之后继续推进队列。
    if owner.legacy_is_running(&request.project)? {
        return Err(AdoptError::LegacyStillRunning {
            project: request.project.clone(),
        }
        .into());
    }

    // 7. 建立受管身份。顺序不能反：先把授权写进表，再起生命周期事务，
    //    否则事务的 `check_binding` 会因为查不到授权而失败。
    let host = managed::host_id(home)?;
    let activation = Lifecycle::open(home)?
        .committed(plugin)?
        .map(|active| active.activation + 1)
        .unwrap_or(1);

    grants::update(home, |table: &mut GrantTable| {
        table
            .grant(
                plugin.as_str(),
                project_id.clone(),
                release,
                &host,
                &request.capabilities,
            )
            .map(|grant| grant.generation)
    })?;

    let transaction = Lifecycle::open(home)?.start(LifecycleRequest::activate(
        plugin.clone(),
        release.clone(),
        activation,
    ))?;

    // 8. 投影与健康检查。任一失败都返回可恢复错误，而不是"大概成了"。
    owner.project(&request.project, release, activation)?;
    if let Err(error) = owner.health_check(&request.project, release) {
        // 说清代价：授权**已经**写进表了，只是这次代次没有提交。含混地说
        // "失败了"会让用户以为什么都没变，然后手动再做一遍。
        return Err(PluginError::RecoveryRequired(format!(
            "granted `{project_key}` but its health check failed: {error}; the backup at {} is \
             intact and the activation is not committed — re-run adoption after fixing the \
             project",
            backup
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        )));
    }

    Ok(Adopted {
        project: project_key,
        transaction,
        activation,
        backup,
    })
}

/// 备份目录名的稳定格式：`legacy-backup-<unix>`。
fn backup_transaction_id(now: chrono::DateTime<chrono::Local>) -> String {
    format!("{BACKUP_SUFFIX}-{}", now.timestamp())
}

/// 体检出来的"会被备份的文件"清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyContents {
    /// `(相对路径, 源文件)`。
    pub files: Vec<(String, PathBuf)>,
}

/// 检查一个旧状态目录。**只读**。
///
/// 与插件侧 `migrate::scan` 的取舍一致：只有认识的文件才进备份清单，
/// 瞬态资源（锁、socket、inbox）一律不进——带进备份会让恢复后出现幽灵持有者。
fn scan_legacy(state_dir: &Path) -> Result<LegacyContents, PluginError> {
    if !state_dir.is_dir() {
        // 一个从未启动过的旧安装就是这样。它不是错误：没有数据可接管，
        // 接管仍然有意义（用户想把这个项目纳入受管），所以返回空清单。
        return Ok(LegacyContents { files: Vec::new() });
    }
    let mut files = Vec::new();
    let entries = std::fs::read_dir(state_dir).map_err(|error| {
        PluginError::Io(format!("could not read {}: {error}", state_dir.display()))
    })?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if is_transient(&name) {
            continue;
        }
        if path.is_file() {
            files.push((name, path));
        }
    }
    // `board.json` 存在却读不出来，是"未知格式"，不能当"没有卡片"继续。
    if let Some((_, board)) = files.iter().find(|(name, _)| name == "board.json") {
        let bytes = std::fs::read(board)
            .map_err(|error| PluginError::Io(format!("{}: {error}", board.display())))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| AdoptError::UnrecognizedData {
                path: board.clone(),
                detail: format!("board.json is not valid JSON: {error}"),
            })?;
        let object = value
            .as_object()
            .ok_or_else(|| AdoptError::UnrecognizedData {
                path: board.clone(),
                detail: "board.json is not an object".to_string(),
            })?;
        for key in object.keys() {
            if key != "cards" && key != "next_order" {
                return Err(AdoptError::UnrecognizedData {
                    path: board.clone(),
                    detail: format!("unknown top-level key `{key}`"),
                }
                .into());
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(LegacyContents { files })
}

fn is_transient(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered.ends_with(".sock")
        || lowered.ends_with(".socket")
        || lowered.ends_with(".pid")
        || lowered.ends_with(".lock")
        || lowered == "operations.json"
        || lowered == "inbox"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    use yi_agent_plugins::ownership::ProjectOwnership;

    fn plugin() -> PluginId {
        PluginId::parse("superpowers-kanban").unwrap()
    }

    fn release() -> ReleaseId {
        ReleaseId {
            plugin: plugin(),
            version: "0.1.0".to_string(),
            target: "darwin-aarch64".to_string(),
            sha256: "a".repeat(64),
        }
    }

    fn at(unix: i64) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(unix, 0)
            .single()
            .expect("valid timestamp")
    }

    /// 可编程的旧 owner：默认"已经停了"，可以让任一步失败。
    #[derive(Default)]
    struct FakeOwner {
        running: Mutex<bool>,
        stop_fails: bool,
        health_fails: bool,
        stops: Mutex<Vec<PathBuf>>,
        healths: Mutex<Vec<PathBuf>>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl FakeOwner {
        fn alive() -> FakeOwner {
            FakeOwner {
                running: Mutex::new(true),
                ..FakeOwner::default()
            }
        }

        fn dead() -> FakeOwner {
            FakeOwner::default()
        }
    }

    impl LegacyOwner for FakeOwner {
        fn legacy_is_running(&self, _project: &Path) -> Result<bool, PluginError> {
            Ok(*self.running.lock().unwrap())
        }

        fn stop_legacy(&self, project: &Path) -> Result<(), PluginError> {
            self.calls.borrow_mut().push("stop");
            self.stops.lock().unwrap().push(project.to_path_buf());
            if self.stop_fails {
                return Err(PluginError::Io("could not stop".to_string()));
            }
            *self.running.lock().unwrap() = false;
            Ok(())
        }

        fn health_check(&self, project: &Path, _release: &ReleaseId) -> Result<(), PluginError> {
            self.healths.lock().unwrap().push(project.to_path_buf());
            if self.health_fails {
                return Err(PluginError::Io("unhealthy".to_string()));
            }
            Ok(())
        }

        fn project(
            &self,
            _project: &Path,
            _release: &ReleaseId,
            _activation: u64,
        ) -> Result<(), PluginError> {
            self.calls.borrow_mut().push("project");
            Ok(())
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
        home: PathBuf,
        project: PathBuf,
        state: PathBuf,
        ownership: Arc<ProjectOwnership>,
    }

    fn fixture(board: &str) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("proj");
        let state = project.join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(state.join("board.json"), board).unwrap();
        let ownership = Arc::new(ProjectOwnership::new(home.join("plugins/owners")));
        Fixture {
            root,
            home,
            project,
            state,
            ownership,
        }
    }

    fn request(fixture: &Fixture) -> AdoptRequest {
        AdoptRequest {
            project: fixture.project.clone(),
            legacy_state_dir: fixture.state.clone(),
            capabilities: vec![Effect::Read, Effect::Write],
            backup: true,
        }
    }

    fn run(
        fixture: &Fixture,
        owner: &dyn LegacyOwner,
        request: &AdoptRequest,
    ) -> Result<Adopted, PluginError> {
        adopt(
            &fixture.home,
            &plugin(),
            &release(),
            request,
            owner,
            &fixture.ownership,
            at(1_700_000_000),
        )
    }

    #[test]
    fn adopting_a_stopped_legacy_install_backs_up_and_grants() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        let owner = FakeOwner::dead();

        let adopted = run(&fixture, &owner, &request(&fixture)).unwrap();

        // 备份真的落地，而且里头有 board.json。
        let backup = adopted.backup.expect("a backup dir");
        assert!(backup.join("board.json").is_file(), "{}", backup.display());
        // 旧数据没被搬走。
        assert!(fixture.state.join("board.json").is_file());
        // 授权写进了表。
        let table = grants::load(&fixture.home).unwrap();
        assert!(
            table
                .get(
                    plugin().as_str(),
                    &ProjectId::parse(&adopted.project).unwrap()
                )
                .is_some()
        );
        // 活跃代次从 1 开始。
        assert_eq!(adopted.activation, 1);
    }

    #[test]
    fn a_running_legacy_process_blocks_adoption_instead_of_being_killed() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        let owner = FakeOwner::alive();

        let error = run(&fixture, &owner, &request(&fixture)).unwrap_err();

        assert!(matches!(error, PluginError::Busy(_)), "{error}");
        // 一次停都没发生：报告阻塞，不替用户做决定。
        assert!(owner.stops.lock().unwrap().is_empty());
        // 也没有留下任何受管登记。
        assert!(grants::load(&fixture.home).unwrap().all().is_empty());
    }

    #[test]
    fn a_live_managed_owner_blocks_adoption() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        let owner = FakeOwner::dead();
        let project_key = fixture.project.to_string_lossy().to_string();
        let _guard = fixture
            .ownership
            .acquire(&project_key, "another-host", 4242)
            .unwrap();

        let error = run(&fixture, &owner, &request(&fixture)).unwrap_err();

        assert!(matches!(error, PluginError::Busy(_)), "{error}");
        assert!(owner.stops.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_board_data_is_refused_before_anything_is_written() {
        let fixture = fixture(r#"{"cards":[],"next_order":0,"futureField":true}"#);
        let owner = FakeOwner::dead();

        let error = run(&fixture, &owner, &request(&fixture)).unwrap_err();

        assert!(matches!(error, PluginError::RecoveryRequired(_)), "{error}");
        // 没有备份目录、没有授权、没有停旧服务——什么都没动。
        assert!(!fixture.home.join("plugins/backups").exists());
        assert!(grants::load(&fixture.home).unwrap().all().is_empty());
        assert!(owner.stops.lock().unwrap().is_empty());
    }

    #[test]
    fn a_missing_project_is_reported_without_guessing_a_nearby_one() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        let owner = FakeOwner::dead();
        let mut request = request(&fixture);
        request.project = fixture.root.path().join("typo");

        let error = run(&fixture, &owner, &request).unwrap_err();

        assert!(matches!(error, PluginError::NotFound(_)), "{error}");
    }

    #[test]
    fn missing_required_capabilities_are_named() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        let owner = FakeOwner::dead();
        let mut request = request(&fixture);
        request.capabilities = vec![Effect::Read];

        let error = run(&fixture, &owner, &request).unwrap_err();

        assert!(matches!(error, PluginError::PermissionDenied(_)), "{error}");
        assert!(error.to_string().contains("Write"), "{error}");
    }

    #[test]
    fn a_process_that_promises_to_stop_but_does_not_blocks_the_adoption() {
        // 最坏的情况：`stop_legacy` 返回成功，但旧服务其实还在。
        struct Stubborn;
        impl LegacyOwner for Stubborn {
            fn legacy_is_running(&self, _: &Path) -> Result<bool, PluginError> {
                Ok(true)
            }
            fn stop_legacy(&self, _: &Path) -> Result<(), PluginError> {
                Ok(())
            }
            fn health_check(&self, _: &Path, _: &ReleaseId) -> Result<(), PluginError> {
                Ok(())
            }
            fn project(&self, _: &Path, _: &ReleaseId, _: u64) -> Result<(), PluginError> {
                Ok(())
            }
        }
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);

        let error = run(&fixture, &Stubborn, &request(&fixture)).unwrap_err();

        // 第一次检查就报了 Busy，连备份都不该发生。
        assert!(matches!(error, PluginError::Busy(_)), "{error}");
        assert!(!fixture.home.join("plugins/backups").exists());
    }

    #[test]
    fn a_failed_health_check_is_recoverable_and_keeps_the_backup() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        let owner = FakeOwner {
            health_fails: true,
            ..FakeOwner::dead()
        };

        let error = run(&fixture, &owner, &request(&fixture)).unwrap_err();

        let message = error.to_string();
        assert!(
            matches!(error, PluginError::RecoveryRequired(_)),
            "{message}"
        );
        // 备份路径在错误里，用户能自己退回去。
        assert!(message.contains("legacy-backup"), "{message}");
        // 备份确实还在盘上。
        let backups: Vec<_> = std::fs::read_dir(fixture.home.join("plugins/backups"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn a_stop_failure_reports_rather_than_claiming_success() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        let owner = FakeOwner {
            stop_fails: true,
            ..FakeOwner::dead()
        };

        let error = run(&fixture, &owner, &request(&fixture)).unwrap_err();

        assert!(matches!(error, PluginError::Io(_)), "{error}");
        // 备份已经存在（先备份后停），但没有任何受管登记。
        assert!(fixture.home.join("plugins/backups").exists());
        assert!(grants::load(&fixture.home).unwrap().all().is_empty());
    }

    #[test]
    fn transient_resources_never_enter_the_backup() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        for name in ["plugin.lock", "board.lock", "superpowers-kanban.sock"] {
            std::fs::write(fixture.state.join(name), "held").unwrap();
        }
        std::fs::create_dir_all(fixture.state.join("inbox")).unwrap();
        let owner = FakeOwner::dead();

        let adopted = run(&fixture, &owner, &request(&fixture)).unwrap();

        let backup = adopted.backup.unwrap();
        for name in [
            "plugin.lock",
            "board.lock",
            "superpowers-kanban.sock",
            "inbox",
        ] {
            assert!(!backup.join(name).exists(), "{name} entered the backup");
        }
        assert!(backup.join("board.json").is_file());
    }

    #[test]
    fn a_project_with_no_legacy_data_can_still_be_adopted() {
        let fixture = fixture(r#"{"cards":[],"next_order":0}"#);
        std::fs::remove_file(fixture.state.join("board.json")).unwrap();
        let owner = FakeOwner::dead();

        let adopted = run(&fixture, &owner, &request(&fixture)).unwrap();

        assert_eq!(adopted.activation, 1);
        // 没东西可备份时不留一个空备份目录冒充"有退路"。
        let manifest = adopted.backup.unwrap().join("manifest.json");
        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        assert_eq!(parsed["entries"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn adoption_records_the_original_boards_projects_order() {
        let board = r#"{"cards":[{"id":"a","spec_path":"s","plan_path":"p","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":3}],"next_order":4}"#;
        let fixture = fixture(board);
        let owner = FakeOwner::dead();

        run(&fixture, &owner, &request(&fixture)).unwrap();

        // 接管不重写旧看板：卡片还在，字段一个没动。
        let bytes = std::fs::read_to_string(fixture.state.join("board.json")).unwrap();
        assert!(bytes.contains("\"order\":3"), "{bytes}");
        assert!(
            bytes.contains("\"id\": \"a\"") || bytes.contains("\"id\":\"a\""),
            "{bytes}"
        );
    }
}
