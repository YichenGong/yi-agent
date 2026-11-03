//! 受管启动入口：registry → 已提交激活 → 受管根 → 进程。
//!
//! # 为什么必须另开一条路，而不是复用监督器的 `command`
//!
//! 监督器清单是**安装那一刻的快照**，里面的 `command` 是插件自述的字符串。
//! 它适合托管"用户自己装的任意常驻进程"，但**不能**用来启动一个有签名、
//! 有授权、有完整性摘要的发布：那样一份被篡改的清单就能把宿主指到任意
//! 可执行文件上，而摘要校验、授权代次、准入屏障全都形同虚设。
//!
//! 受管插件的程序路径只能由 [`resolve`] 推导：注册表记录的安装目录 +
//! 包内清单声明的入口。它不读监督器清单，也不依赖用户 `PATH`。
//!
//! # 清单从包里读，不从调用方接
//!
//! 发布清单绑定在签名包里，因此归档摘要就是它的身份。让调用方"传一份清单
//! 进来"会凭空多出一个可以配错的输入：A 版的代码 + B 版的能力声明，宿主
//! 会按 B 放行、执行 A。这里改为直接读安装目录里的 `plugin.json`，它的
//! 字节已被 `verify_installed` 的逐文件摘要钉住。
//!
//! # 保留环境变量
//!
//! `host-v1.json` 的 `reservedEnv` 声明了宿主拥有的变量。插件若能把
//! `YI_AGENT_HOME` 改到别处，就能让宿主读写另一个实例根——等于绕过整份
//! 授权模型。因此插件提供的环境项**一律不允许**命中保留名：命中即拒绝
//! 启动，而不是悄悄丢弃。悄悄丢弃会让插件"看起来配好了、实际没生效"，
//! 那比直接失败更难排查。

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use yi_agent_plugins::{
    Binding, Effect, GrantTable, Lifecycle, Manifest, PluginError, PluginId, ProjectId, Registry,
    bundle, journal::JournalStore, locks, ownership::ProjectOwnership,
};

/// 宿主拥有的环境变量名。
///
/// 与 `plugins/contracts/fixtures/host-v1.json` 的 `reservedEnv` 逐字一致。
/// 两者由 `reserved_env_matches_the_published_contract` 钉住：契约改了而这里
/// 没改，宿主就会允许插件覆盖一个本该保留的变量。
pub const RESERVED_ENV: [&str; 3] = [
    "YI_AGENT_HOME",
    "YI_AGENT_PLUGIN_INSTANCE",
    "YI_AGENT_PLUGIN_BINDING",
];

/// 包内发布清单的固定路径。
///
/// 固定而不是可配置：可配置就等于让调用方指定"信哪份清单"，而清单正是
/// 能力声明的唯一来源。
pub const MANAGED_MANIFEST: &str = "plugin.json";

/// 一个已解析、已校验的受管安装。
#[derive(Debug, Clone)]
pub struct ManagedInstall {
    /// 受管根下的安装目录（绝对路径）。
    pub directory: PathBuf,
    /// 本次启动实际对应的发布身份。
    pub release: yi_agent_plugins::ReleaseId,
    /// 已提交的激活代次。
    pub activation: u64,
    /// 包内清单，已通过摘要与身份校验。
    pub manifest: Manifest,
}

impl ManagedInstall {
    /// 程序绝对路径。`None` 表示该发布是纯视图插件，没有原生程序。
    pub fn program(&self) -> Option<PathBuf> {
        self.manifest
            .program
            .as_ref()
            .map(|program| self.directory.join(&program.entry))
    }
}

/// 宿主实例的稳定标识。
///
/// 同一实例跨重启不变（由实例根路径派生），不同实例不同。用路径摘要而不是
/// 随机 UUID 落盘，是因为后者需要一个"第一次运行"分支，而那个分支在并发
/// 启动时会产生两个不同的 id——授权正是按这个 id 校验的。
pub fn host_id(instance_home: &Path) -> Result<String, PluginError> {
    let canonical = std::fs::canonicalize(instance_home).map_err(|error| {
        PluginError::Io(format!(
            "cannot canonicalize the instance root {}: {error}",
            instance_home.display()
        ))
    })?;
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Ok(format!("host-{hex}"))
}

/// registry 里记录安装目录的字段是**相对实例根**的路径。
///
/// 这里再校验一次它没有逃出实例根：注册表是宿主写的，但"宿主写的"不等于
/// "永远正确"，一份被手工改过的注册表不该能把程序指到 `/usr/bin`。
fn managed_directory(instance_home: &Path, relative: &str) -> Result<PathBuf, PluginError> {
    let root = std::fs::canonicalize(instance_home).map_err(|error| {
        PluginError::Io(format!(
            "cannot canonicalize the instance root {}: {error}",
            instance_home.display()
        ))
    })?;
    let candidate = root.join(relative);
    let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
        PluginError::Integrity(format!(
            "recorded installation directory {} is unavailable: {error}",
            candidate.display()
        ))
    })?;
    if !resolved.starts_with(&root) {
        return Err(PluginError::Integrity(format!(
            "recorded installation directory {} resolves outside the instance root {}",
            resolved.display(),
            root.display()
        )));
    }
    Ok(resolved)
}

/// 解析一个受管插件的启动目标。
///
/// 顺序即安全边界，不能调换：
/// 1. 已提交激活必须存在。没有激活就没有可执行版本。
/// 2. 注册表必须记着那个版本**和那个摘要**的安装；`(version, sha256)` 是
///    发布身份，只对上其中一个等于允许"换过内容的同版本"。
/// 3. 安装目录必须在实例根内，且逐文件摘要与安装时一致。
/// 4. 包内清单解析后必须**自称**为该已提交发布，且声明入口的摘要与文件
///    实际字节一致。清单在签名包里，但仍核对一次：一份从别处捡来的
///    `plugin.json` 会让能力声明与代码脱钩。
pub fn resolve(instance_home: &Path, plugin: &PluginId) -> Result<ManagedInstall, PluginError> {
    let committed = Lifecycle::open(instance_home)?
        .committed(plugin)?
        .ok_or_else(|| {
            PluginError::NotFound(format!(
                "plugin `{}` has no committed activation; install and activate a release first",
                plugin.as_str()
            ))
        })?;

    let record = Registry::open(instance_home)?
        .list()?
        .into_iter()
        .find(|record| &record.plugin == plugin)
        .ok_or_else(|| {
            PluginError::NotFound(format!(
                "plugin `{}` is not registered in this instance",
                plugin.as_str()
            ))
        })?;
    let installed = record
        .installed
        .iter()
        .find(|version| {
            version.version == committed.release.version
                && version.sha256 == committed.release.sha256
        })
        .ok_or_else(|| {
            PluginError::NotFound(format!(
                "plugin `{}` version `{}` with digest {} is not installed; the registry records \
                 a different release for that version",
                plugin.as_str(),
                committed.release.version,
                committed.release.sha256
            ))
        })?;

    let directory = managed_directory(instance_home, &installed.directory)?;
    bundle::verify_installed(&directory)?;

    let manifest_path = directory.join(MANAGED_MANIFEST);
    let bytes = std::fs::read(&manifest_path).map_err(|error| {
        PluginError::Integrity(format!(
            "the installation has no readable release manifest at {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest = Manifest::parse(&bytes)?;

    if manifest.plugin != committed.release.plugin
        || manifest.version != committed.release.version
        || manifest.target != committed.release.target
    {
        return Err(PluginError::StaleBinding(format!(
            "the packaged manifest describes {}/{}/{} but activation {} committed {}/{}/{}",
            manifest.plugin.as_str(),
            manifest.version,
            manifest.target,
            committed.activation,
            committed.release.plugin.as_str(),
            committed.release.version,
            committed.release.target
        )));
    }

    if let Some(program) = manifest.program.as_ref() {
        bundle::validate_entry_name(&program.entry)?;
        let path = directory.join(&program.entry);
        let bytes = std::fs::read(&path).map_err(|error| {
            PluginError::Integrity(format!(
                "declared program entry {} is missing: {error}",
                path.display()
            ))
        })?;
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != program.digest {
            return Err(PluginError::Integrity(format!(
                "declared program entry {} has digest {actual}, the manifest declares {}",
                path.display(),
                program.digest
            )));
        }
        // 安装时只给声明入口设置受控执行位。这里复核一次：摘要一致但执行位
        // 被去掉，说明安装目录被动过。
        let mode = std::fs::metadata(&path)
            .map_err(|error| PluginError::Integrity(format!("{}: {error}", path.display())))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(PluginError::Integrity(format!(
                "declared program entry {} is not executable",
                path.display()
            )));
        }
    }

    Ok(ManagedInstall {
        directory,
        release: committed.release,
        activation: committed.activation,
        manifest,
    })
}

/// 受管启动的两种形态。
///
/// 它们的效果来源**不同**，因此不能合并成一条路径：
///
/// - [`LaunchMode::Cli`] 走签名清单的 `cli` 表：verb → effect。授权检查的是
///   这次调用**实际要做的事**（读还是写）。
/// - [`LaunchMode::Resident`] 是常驻后端进程。它不是一次 CLI 调用，没有 verb，
///   效果由协议固定为 `Launch`（`plugin-host-protocol.md` §5.2 明确
///   `Launch`/`QuotaWrite`/`Resident` 不要求 backing method）。若拿常驻进程的
///   argv 去查 verb，任何后端都启动不了；反过来若把它默认为 `Read`，一个
///   "只读"授权就能拉起一个会写数据的常驻进程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// 一次性 CLI 调用，argv 首项是清单声明的 verb。
    Cli,
    /// 常驻后端进程，argv 由宿主装配，效果固定为 `Launch`。
    Resident,
}

/// 一次受管启动的完整输入。
pub struct LaunchRequest<'a> {
    /// 这次启动是哪一种形态。决定效果从哪来，见 [`LaunchMode`]。
    pub mode: LaunchMode,
    pub instance_home: &'a Path,
    pub install: &'a ManagedInstall,
    pub binding: &'a Binding,
    /// 由宿主在**本项目**目录下确定的工作目录。
    pub workdir: &'a Path,
    /// `--` 之后的参数，原样作为 argv 传递，不拼接 shell 字符串。
    pub argv: &'a [std::ffi::OsString],
    /// 插件自己声明的环境项。命中保留名即拒绝。
    pub plugin_env: &'a [(String, String)],
    pub grants: &'a GrantTable,
}

/// 校验并构造一个可以 spawn 的命令。
///
/// 所有拒绝都发生在 spawn 之前：一条被拒绝的启动不能留下半启动的进程。
pub fn prepare(request: &LaunchRequest<'_>) -> Result<Command, PluginError> {
    let home = request.instance_home;
    let manifest = &request.install.manifest;
    let plugin = &manifest.plugin;

    // --- 1. 绑定必须指向**这个**发布 ---
    //
    // 绑定里四项（宿主、发布、激活代次、项目授权代次）缺一不可。少检查
    // 任何一项，一份旧绑定就能在升级后继续用，等于绕过了"能力增加须重新
    // 确认"。
    if request.binding.release.plugin != *plugin {
        return Err(PluginError::PermissionDenied(format!(
            "binding is for plugin `{}`, not `{}`",
            request.binding.release.plugin.as_str(),
            plugin.as_str()
        )));
    }
    if request.binding.release.version != request.install.release.version
        || request.binding.release.target != request.install.release.target
        || request.binding.release.sha256 != request.install.release.sha256
    {
        return Err(PluginError::StaleBinding(format!(
            "binding names release {}@{} ({}), the installed release is {}@{} ({})",
            request.binding.release.version,
            request.binding.release.target,
            request.binding.release.sha256,
            request.install.release.version,
            request.install.release.target,
            request.install.release.sha256
        )));
    }
    // 激活代次这里是**第一道**检查，`Lifecycle::check_binding` 是第二道。
    //
    // 两处判同一件事看似重复，但它们读的东西不同：这里比的是调用方传进来的
    // 绑定与已解析的安装，`check_binding` 读磁盘上的生命周期状态与日志。
    // 后者能发现"绑定与安装碰巧都旧"的情形；前者能在读任何文件之前就给出
    // 一条同时包含两个代次、便于定位的错误。删掉任何一道都不会让红例变绿
    // （另一道会拦），这正是我们要的分层，而不是可以随手合并的重复。
    if request.binding.activation != request.install.activation {
        return Err(PluginError::StaleBinding(format!(
            "binding carries activation {}, the plugin is at {}",
            request.binding.activation, request.install.activation
        )));
    }

    // --- 2. 宿主身份：授权记在执行宿主上，别的宿主不能拿来用 ---
    let expected_host = host_id(home)?;
    if request.binding.host_id != expected_host {
        return Err(PluginError::PermissionDenied(format!(
            "binding belongs to host `{}`, this instance is `{expected_host}`",
            request.binding.host_id
        )));
    }

    // --- 3. 效果由**签名清单**决定，不由调用方声明 ---
    //
    // 两种形态的来源不同但结论都必须来自磁盘上的签名物：CLI 来自清单的
    // `cli` 表，常驻来自协议固定的 `Launch`。调用方无权声明 effect。
    let effect = match request.mode {
        LaunchMode::Cli => effect_for_argv(manifest, request.argv)?,
        LaunchMode::Resident => Effect::Launch,
    };
    request.grants.check(
        plugin.as_str(),
        &request.binding.project,
        effect,
        &expected_host,
        request.binding.grant,
    )?;

    // --- 4. 已提交激活 + 准入是否开放 ---
    //
    // 候选代次与未结束事务都在这一条里拒绝，不需要另写一份屏障判断。
    Lifecycle::open(home)?.check_binding(request.binding, effect)?;

    build_command(request, effect)
}

/// argv 对应的效果，由**签名清单**的 `cli` 表决定。
///
/// 未声明的 verb 一律拒绝。若默认成 `Read`，一个自称"只读"的调用方就能在
/// 没有 `Write` 授权时改数据——设计明确禁止"客户端把 write 标成 read"。
fn effect_for_argv(
    manifest: &Manifest,
    argv: &[std::ffi::OsString],
) -> Result<Effect, PluginError> {
    let verb = argv.first().and_then(|v| v.to_str()).ok_or_else(|| {
        PluginError::PermissionDenied(
            "no CLI verb was given; the effect of an empty command is undefined".to_string(),
        )
    })?;
    if argv.iter().any(|arg| arg == "--") {
        // 宿主已经用第一个 `--` 切开了自己的参数与插件 argv。再出现一个
        // 说明调用方以为还有一层转义，交给插件只会造成歧义。
        return Err(PluginError::PermissionDenied(
            "a second `--` is not allowed after the host separator".to_string(),
        ));
    }
    manifest
        .cli
        .iter()
        .find(|entry| entry.verb == verb)
        .map(|entry| entry.effect)
        .ok_or_else(|| {
            PluginError::PermissionDenied(format!(
                "plugin `{}` does not declare the verb `{verb}`; an undeclared verb has no \
                 declared effect, and defaulting to read would let a write through a read grant",
                manifest.plugin.as_str()
            ))
        })
}

fn build_command(request: &LaunchRequest<'_>, effect: Effect) -> Result<Command, PluginError> {
    let program = request.install.program().ok_or_else(|| {
        PluginError::NotSupported(format!(
            "plugin `{}` declares no native program; it cannot be launched as a process",
            request.install.manifest.plugin.as_str()
        ))
    })?;

    let mut command = Command::new(program);
    // argv 逐项透传，不经过 shell：空格、中文、分号都只是字符。
    command.args(request.argv);
    command.current_dir(request.workdir);
    command.stdin(Stdio::null());

    // 插件环境项：命中保留名即**拒绝启动**，而不是悄悄丢弃。悄悄丢弃会让
    // 插件"看起来配好了、实际没生效"，比直接失败更难排查。
    for (name, value) in request.plugin_env {
        if RESERVED_ENV.contains(&name.as_str()) {
            return Err(PluginError::PermissionDenied(format!(
                "plugin `{}` may not set the reserved environment variable `{name}`; it belongs \
                 to the host and would redirect the instance root",
                request.install.manifest.plugin.as_str()
            )));
        }
        command.env(name, value);
    }

    // 保留变量由宿主**最后**写。即使上面的检查漏掉一项，宿主写入也覆盖它。
    let home = request.instance_home;
    command.env("YI_AGENT_HOME", home);
    command.env("YI_AGENT_PLUGIN_INSTANCE", host_id(home)?);
    command.env(
        "YI_AGENT_PLUGIN_BINDING",
        format!(
            "{}:{}:{}:{}",
            request.binding.project.as_str(),
            request.binding.grant,
            request.binding.activation,
            effect.as_str()
        ),
    );
    Ok(command)
}

/// 维护闸门：**每次决策都从磁盘重读**。
///
/// 用 trait 而不是一个 `bool` 参数，是为了让"缓存上一次结论"这件事在类型上
/// 就做不到。IPC 通知丢失过一次，缓存就会把"正在维护"记成"没在维护"，
/// 于是后端被重新拉起、迁移被两个写者同时执行。
pub trait MaintenanceGate {
    /// 该项目所属插件的生命周期事务是否尚未收尾。
    fn is_active(&self, plugin: &PluginId) -> Result<bool, PluginError>;
}

/// 磁盘上的 journal 就是闸门本身。
///
/// 只看日志存在且 `phase` 不是 `done`。不看内存里的 `Phase`：进程重启后
/// 内存判据消失，一个"停在 projecting"的事务会立刻变成"准入开放"。
///
/// # 为什么每次 `is_active` 都重新读盘
///
/// 这正是"RPC 通知丢失不能成为放行理由"的落点。维护开始的通知可能丢、可能
/// 在别的进程里发生，内存里的状态因此**永远不可信**。每次决策都读一次磁盘
/// 上的权威记录：代价是几百字节的读取，换来的是不需要相信任何一条通知。
///
/// # 为什么构造不预加载
///
/// `JournalStore::open` 会一次性读全部日志，一份损坏的日志会让**整个**闸门
/// 构造失败。而这里的正确行为不是"起不来"，是"每个插件各自 fail closed"：
/// 读不到的那一个保持停止，其它照常。所以只存实例根，读失败在
/// [`MaintenanceGate::is_active`] 里作为错误冒出去，由调用方保守处理。
pub struct JournalGate {
    instance_home: PathBuf,
}

impl JournalGate {
    pub fn open(instance_home: impl Into<PathBuf>) -> Result<Self, PluginError> {
        Ok(Self {
            instance_home: instance_home.into(),
        })
    }
}

impl MaintenanceGate for JournalGate {
    fn is_active(&self, plugin: &PluginId) -> Result<bool, PluginError> {
        // `at` 不做全量预校验：别的插件的坏日志不该让这个插件也起不来。
        // `load` 仍会在本插件的日志损坏时返回 `RecoveryRequired`。
        Ok(JournalStore::at(&self.instance_home)
            .load(plugin)?
            .map(|journal| !journal.phase.admission_open())
            .unwrap_or(false))
    }
}

/// 重启决策。用枚举而不是 `bool`：UI 需要区分"维护中"与"后端故障"，
/// 后者必须显示成故障，**不能显示成空数据**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// 可以重启。
    Restart,
    /// 维护事务未收尾，后端必须保持停止。
    RefuseMaintenance,
    /// 熔断器打开：不再自动拉起。
    RefuseBreaker,
}

/// 注入式时钟。熔断测试必须能在毫秒内走完 10 分钟，不能 `sleep`。
pub trait Clock {
    fn now_ms(&self) -> u64;
}

/// 生产时钟。
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// 一个被托管进程的启动退避。数值来自 G2 §7：1 s → 2 s → 4 s → 8 s → 16 s。
pub const BACKOFF_STEPS_MS: [u64; 5] = [1_000, 2_000, 4_000, 8_000, 16_000];

/// 第 `attempt` 次连续重启的退避（`attempt` 从 0 开始），上限 16 s。
pub fn backoff_ms(attempt: u32) -> u64 {
    let index = (attempt as usize).min(BACKOFF_STEPS_MS.len() - 1);
    BACKOFF_STEPS_MS[index]
}

/// 崩溃循环熔断器。数值来自 G2 §7，宿主可配置但不得放宽。
#[derive(Debug, Clone)]
pub struct Breaker {
    /// 窗口内允许的崩溃次数，达到即熔断。
    threshold: u32,
    /// 滑动窗口长度。
    window_ms: u64,
    /// 熔断后"连续无崩溃"多久自动恢复。
    recovery_ms: u64,
    /// 窗口内的崩溃时刻（升序）。
    crashes: VecDeque<u64>,
    /// 熔断时刻；`None` 表示未熔断。
    opened_at: Option<u64>,
}

impl Default for Breaker {
    fn default() -> Self {
        Self::new(5, 10 * 60 * 1000, 10 * 60 * 1000)
    }
}

impl Breaker {
    pub fn new(threshold: u32, window_ms: u64, recovery_ms: u64) -> Self {
        Self {
            threshold: threshold.max(1),
            window_ms,
            recovery_ms,
            crashes: VecDeque::new(),
            opened_at: None,
        }
    }

    /// 记录一次崩溃。
    pub fn record_crash(&mut self, clock: &dyn Clock) {
        let now = clock.now_ms();
        self.crashes.push_back(now);
        while let Some(oldest) = self.crashes.front() {
            if now.saturating_sub(*oldest) > self.window_ms {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
        if self.opened_at.is_none() && self.crashes.len() as u32 >= self.threshold {
            self.opened_at = Some(now);
        }
    }

    /// 到达恢复窗口就关闭熔断。**只在真的没有新崩溃时**生效。
    ///
    /// 判据是"距最后一次崩溃已过恢复窗口"，而不是"这次探活成功"。崩溃循环里
    /// 每次都能启动成功、探活也都能通过，把探活成功当成恢复会让熔断永远不
    /// 生效——那正是它要防的无限重启。
    pub fn maybe_recover(&mut self, clock: &dyn Clock) {
        let Some(opened_at) = self.opened_at else {
            return;
        };
        let now = clock.now_ms();
        let last_crash = self.crashes.back().copied().unwrap_or(opened_at);
        if now.saturating_sub(last_crash) >= self.recovery_ms {
            self.reset();
        }
    }

    /// 别名，供"后端刚刚探活成功"的调用点使用：它与任何一次时间推进一样，
    /// 只是给 [`Breaker::maybe_recover`] 一个检查恢复窗口的机会。
    pub fn record_healthy(&mut self, clock: &dyn Clock) {
        self.maybe_recover(clock);
    }

    /// 手动恢复（用户点了"重试"）。
    pub fn reset(&mut self) {
        self.crashes.clear();
        self.opened_at = None;
    }

    pub fn is_open(&self) -> bool {
        self.opened_at.is_some()
    }

    /// 熔断期间的崩溃次数，供状态显示。
    pub fn crash_count(&self) -> usize {
        self.crashes.len()
    }
}

/// 后端状态。`Failed` 必须被渲染成故障，不能当作"没有数据"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    Running,
    Failed,
}

/// 守护一个项目的后端：持有项目所有权，决定能否重启。
///
/// 关键不变量：**所有权在重启期间不释放**。释放再抢回来会留出一个窗口，
/// 另一个实例可能在同一刻抢到同一项目，然后两个守护者各自拉起一个后端。
pub struct Guardian<'a> {
    ownership: &'a ProjectOwnership,
    project_key: String,
    owner: String,
    /// 持有到 `Guardian` 结束；字段本身不被读取，存在即持有。
    _guard: yi_agent_plugins::OwnerGuard<'a>,
    breaker: Breaker,
}

impl<'a> Guardian<'a> {
    /// 取得项目所有权。第二个实例立即 `Busy`，且错误里带持有者名字。
    pub fn acquire(
        ownership: &'a ProjectOwnership,
        project_key: &str,
        owner: &str,
        pid: u32,
        breaker: Breaker,
    ) -> Result<Self, PluginError> {
        let guard = ownership.acquire(project_key, owner, pid)?;
        Ok(Self {
            ownership,
            project_key: project_key.to_string(),
            owner: owner.to_string(),
            _guard: guard,
            breaker,
        })
    }

    pub fn project_key(&self) -> &str {
        &self.project_key
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// 后端退出后是否允许自动拉起。
    ///
    /// 顺序是有意的：
    /// 1. **维护闸门先判**。维护期间的重启被设计禁止，与崩溃频率无关；先判
    ///    熔断会让"维护中"被误报成"后端故障"。
    /// 2. 每次决策都重新问闸门，**不缓存**上一次结论。IPC 通知丢过一次，
    ///    缓存就会把"正在维护"记成"没在维护"。
    /// 3. 熔断器只在这里检查恢复窗口，它是唯一的时间推进点，不能漏。
    pub fn decide_restart(
        &mut self,
        gate: &dyn MaintenanceGate,
        plugin: &PluginId,
        clock: &dyn Clock,
    ) -> Result<RestartDecision, PluginError> {
        if gate.is_active(plugin)? {
            return Ok(RestartDecision::RefuseMaintenance);
        }
        self.breaker.maybe_recover(clock);
        if self.breaker.is_open() {
            return Ok(RestartDecision::RefuseBreaker);
        }
        Ok(RestartDecision::Restart)
    }

    /// 记录一次后端崩溃。
    pub fn record_crash(&mut self, clock: &dyn Clock) {
        self.breaker.record_crash(clock);
    }

    /// 记录一次健康探活。
    pub fn record_healthy(&mut self, clock: &dyn Clock) {
        self.breaker.record_healthy(clock);
    }

    pub fn backend_state(&self) -> BackendState {
        if self.breaker.is_open() {
            BackendState::Failed
        } else {
            BackendState::Running
        }
    }

    pub fn breaker(&self) -> &Breaker {
        &self.breaker
    }

    pub fn ownership(&self) -> &ProjectOwnership {
        self.ownership
    }
}

/// 受管模式下由宿主决定启动什么，清单只提供开关与目录。
///
/// # 为什么清单的 `command` 在这里必须失效
///
/// 监督器清单是插件自述的快照。受管插件有签名、有授权、有完整性摘要，
/// 若仍按清单里的 `command` 启动，一份被篡改的清单就能把宿主指到任意
/// 可执行文件上，把摘要与授权检查全部绕过。所以受管模式下程序路径只来自
/// [`resolve`]，清单里的 `command` 与 `args` 一律不参与。
///
/// # 为什么每次启动都重新解析
///
/// 不缓存 `ManagedInstall`：升级会在两次 restart 之间改变"当前激活版本"，
/// 缓存会让守护者一直拉起旧版本。解析本身是几次小文件读取，比起一次错误
/// 的启动便宜得多。
pub struct ManagedGuard {
    pub instance_home: PathBuf,
    pub plugin: PluginId,
    /// 维护闸门。**每次** spawn 前都问一次，不缓存结论。
    pub gate: Box<dyn MaintenanceGate + Send>,
}

impl ManagedGuard {
    pub fn new(
        instance_home: impl Into<PathBuf>,
        plugin: PluginId,
        gate: Box<dyn MaintenanceGate + Send>,
    ) -> Self {
        Self {
            instance_home: instance_home.into(),
            plugin,
            gate,
        }
    }

    /// 维护期间是否必须保持停止。
    ///
    /// 单独暴露，是因为监督器在"进程还活着"的分支上也要判它：维护开始后
    /// 应当停止后端，而不是等它自己崩。只看重启路径会留下一个正在写数据的
    /// 进程，而迁移正是要独占它。
    pub fn blocks_launch(&self) -> Result<bool, PluginError> {
        self.gate.is_active(&self.plugin)
    }

    /// 构造一个受管命令；**任何**一步不通过都不启动。
    ///
    /// 授权不是可选装饰：没有 grant 就没有 binding，没有 binding 就没有
    /// 可以执行的东西。这里把"无授权不执行"做成类型上的必经步骤——不是
    /// 一个可以被调用方跳过的 if。
    pub fn command(
        &self,
        project: &ProjectId,
        argv: &[std::ffi::OsString],
    ) -> Result<Command, PluginError> {
        self.command_in(project, argv, &self.instance_home)
    }

    /// 同上，但工作目录由调用方给。
    ///
    /// 常驻后端必须跑在**项目**目录里：它按相对路径找项目数据，而
    /// 实例根是宿主自己的状态目录。默认用实例根只是为了让"没有项目上下文"
    /// 的调用方不必编一个路径；监督器那条路径必须传真实的项目目录。
    pub fn command_in(
        &self,
        project: &ProjectId,
        argv: &[std::ffi::OsString],
        workdir: &Path,
    ) -> Result<Command, PluginError> {
        if self.blocks_launch()? {
            return Err(PluginError::Draining(format!(
                "plugin `{}` is under a maintenance transaction; it must stay stopped until the \
                 transaction reaches done",
                self.plugin.as_str()
            )));
        }

        let install = resolve(&self.instance_home, &self.plugin)?;
        let table = grants::load(&self.instance_home)?;
        let host = host_id(&self.instance_home)?;
        let grant = table
            .get(self.plugin.as_str(), project)
            .ok_or_else(|| {
                PluginError::PermissionDenied(format!(
                    "plugin `{}` is not authorized for project `{}`; refusing to launch it",
                    self.plugin.as_str(),
                    project.as_str()
                ))
            })?
            .clone();

        let binding = Binding {
            host_id: host,
            release: install.release.clone(),
            activation: install.activation,
            project: project.clone(),
            grant: grant.generation,
        };

        let request = LaunchRequest {
            // 常驻后端：它没有 CLI verb，效果由协议固定为 `Launch`。
            mode: LaunchMode::Resident,
            instance_home: &self.instance_home,
            install: &install,
            binding: &binding,
            workdir,
            argv,
            plugin_env: &[],
            grants: &table,
        };
        prepare(&request)
    }
}

/// 授权表的落盘位置与读写。
///
/// 只在这一层实现，是因为它是**宿主级**状态：CLI 的 enable-project /
/// change-grant / disable-project 与进程启动前的检查必须看到同一份数据。
/// 损坏即报错，绝不当空表——与注册表同一条理由：把损坏当空表，下一次
/// 写会把全部授权抹掉。
pub mod grants {
    use super::*;

    pub fn path(instance_home: &Path) -> PathBuf {
        instance_home.join("plugins").join("grants.json")
    }

    fn lock_path(instance_home: &Path) -> PathBuf {
        instance_home.join("plugins").join("grants.json.lock")
    }

    /// 读取授权表。缺失即空表；损坏即 `RecoveryRequired`。
    pub fn load(instance_home: &Path) -> Result<GrantTable, PluginError> {
        let file = path(instance_home);
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GrantTable::new());
            }
            Err(error) => return Err(PluginError::Io(format!("{}: {error}", file.display()))),
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            PluginError::RecoveryRequired(format!(
                "grant table at {} is unreadable ({error}); refusing to treat it as empty, \
                 because the next write would erase every authorization",
                file.display()
            ))
        })
    }

    /// 读-改-写，整个序列在短锁内完成。
    ///
    /// 不持锁做任何 IO 等待：拿锁去等进程退出或等网络，会让所有实例的授权
    /// 读取一起阻塞（G2 §5.1）。
    pub fn update<R>(
        instance_home: &Path,
        mutate: impl FnOnce(&mut GrantTable) -> Result<R, PluginError>,
    ) -> Result<R, PluginError> {
        let _guard = locks::acquire(&lock_path(instance_home), std::time::Duration::from_secs(5))?;
        let mut table = load(instance_home)?;
        let outcome = mutate(&mut table)?;
        write_atomic(instance_home, &table)?;
        Ok(outcome)
    }

    fn write_atomic(instance_home: &Path, table: &GrantTable) -> Result<(), PluginError> {
        let file = path(instance_home);
        let parent = file.parent().expect("grants path has a parent");
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(table)
            .map_err(|e| PluginError::Io(format!("could not serialize the grant table: {e}")))?;
        let temp = parent.join(format!(".grants.{}.tmp", std::process::id()));
        {
            use std::io::Write;
            let mut handle = std::fs::File::create(&temp)?;
            handle.write_all(&bytes)?;
            handle.sync_all()?;
        }
        std::fs::rename(&temp, &file)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    struct MutableClock(std::cell::Cell<u64>);

    impl Clock for MutableClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }

    #[test]
    fn reserved_env_matches_the_published_contract() {
        // 契约改了而这里没改，宿主就会允许插件覆盖一个本该保留的变量。
        // crate 在 `yi-agent-rs/crates/<name>`，仓库根要再上溯一级。
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("plugins/contracts/fixtures/host-v1.json");
        let bytes = std::fs::read(&root).expect("the published host contract must be readable");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        let declared: Vec<String> = value["reservedEnv"]
            .as_array()
            .expect("reservedEnv is an array")
            .iter()
            .map(|v| v.as_str().expect("strings").to_string())
            .collect();
        assert_eq!(declared, RESERVED_ENV);
    }

    #[test]
    fn the_breaker_opens_only_at_the_threshold() {
        let clock = MutableClock(std::cell::Cell::new(0));
        let mut breaker = Breaker::default();
        for _ in 0..4 {
            breaker.record_crash(&clock);
        }
        assert!(
            !breaker.is_open(),
            "4 crashes must not trip a 5-crash breaker"
        );
        breaker.record_crash(&clock);
        assert!(
            breaker.is_open(),
            "the 5th crash inside the window trips it"
        );
    }

    #[test]
    fn crashes_outside_the_window_do_not_accumulate() {
        // 十小时里崩溃 4 次不是崩溃循环。窗口外的旧记录必须滑出。
        let clock = MutableClock(std::cell::Cell::new(0));
        let mut breaker = Breaker::default();
        for step in 0..4 {
            clock.0.set(step * 20 * 60 * 1000);
            breaker.record_crash(&clock);
        }
        assert!(!breaker.is_open());
        assert_eq!(breaker.crash_count(), 1);
    }

    #[test]
    fn the_breaker_recovers_only_after_a_crash_free_window() {
        let clock = MutableClock(std::cell::Cell::new(0));
        let mut breaker = Breaker::default();
        for _ in 0..5 {
            breaker.record_crash(&clock);
        }
        assert!(breaker.is_open());

        // 刚起来就探活成功不算恢复：崩溃循环里每次都能启动成功。
        clock.0.set(1000);
        breaker.record_healthy(&clock);
        assert!(
            breaker.is_open(),
            "one healthy probe is not a crash-free window"
        );

        clock.0.set(10 * 60 * 1000);
        breaker.record_healthy(&clock);
        assert!(
            !breaker.is_open(),
            "ten crash-free minutes close the breaker"
        );
    }

    #[test]
    fn a_manual_reset_closes_the_breaker_immediately() {
        let clock = FixedClock(0);
        let mut breaker = Breaker::default();
        for _ in 0..5 {
            breaker.record_crash(&clock);
        }
        assert!(breaker.is_open());
        breaker.reset();
        assert!(!breaker.is_open());
        assert_eq!(breaker.crash_count(), 0);
    }

    #[test]
    fn the_backoff_is_capped_at_sixteen_seconds() {
        assert_eq!(backoff_ms(0), 1_000);
        assert_eq!(backoff_ms(1), 2_000);
        assert_eq!(backoff_ms(2), 4_000);
        assert_eq!(backoff_ms(3), 8_000);
        assert_eq!(backoff_ms(4), 16_000);
        // 不能无限增长：再久也停在 16 s。
        assert_eq!(backoff_ms(9), 16_000);
        assert_eq!(backoff_ms(u32::MAX), 16_000);
    }

    #[test]
    fn a_failed_backend_is_reported_as_failed_not_as_running() {
        let clock = FixedClock(0);
        let tmp = tempfile::tempdir().unwrap();
        let ownership = ProjectOwnership::new(tmp.path().join("owners"));
        let mut guardian =
            Guardian::acquire(&ownership, "/p", "host-a", 1, Breaker::default()).unwrap();
        assert_eq!(guardian.backend_state(), BackendState::Running);
        for _ in 0..5 {
            guardian.record_crash(&clock);
        }
        assert_eq!(
            guardian.backend_state(),
            BackendState::Failed,
            "an open breaker must render as a backend failure, never as empty data"
        );
    }

    #[test]
    fn a_second_instance_cannot_guard_the_same_project() {
        // 两个实例各自持有一份 ownership（不同目录 = 不同实例根）不行，
        // 那本来就是两个配额域；这里模拟的是**同一台机器**上的两个宿主
        // 进程共用一个 ownership 根，但各自是不同的 Guardian。
        let tmp = tempfile::tempdir().unwrap();
        let ownership = ProjectOwnership::new(tmp.path().join("owners"));
        let first = Guardian::acquire(&ownership, "/p", "host-a", 1, Breaker::default()).unwrap();
        let error = match Guardian::acquire(&ownership, "/p", "host-b", 2, Breaker::default()) {
            Err(error) => error,
            Ok(_) => panic!("a second owner must not take a project that is already owned"),
        };
        assert_eq!(error.kind(), "Busy");
        let message = error.to_string();
        assert!(
            message.contains("host-a"),
            "the Busy error must name the live holder, got: {message}"
        );
        drop(first);
    }
}
