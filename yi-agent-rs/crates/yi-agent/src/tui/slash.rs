//! Slash command definitions and popup state for the TUI.

use crate::control_commands::{CommandSpec, ControlCommand};
use crate::tui::runtime_prefs::RuntimePreference;

/// The seven reasoning-effort tiers, in ascending order.
///
/// Single source of truth for the TUI's `/effort` validation. The app-server
/// (`model_rpc::upsert`, `server.rs`'s `thread/setReasoningEffort`) and the
/// model catalog use the same set; a wrong value here would be caught by the
/// driver's own rebuild, but a typo would then surface as an opaque error
/// instead of the usage line.
pub const REASONING_EFFORT_TIERS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A slash command the user can invoke from the TUI input.
///
/// Adds metadata (description, arg requirements) needed for the popup UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Quit,
    Clear,
    Model,
    Effort,
    Cost,
    Compact,
    Config,
    Help,
    Agents,
    Agent,
    Events,
    Diff,
    Mailbox,
    Message,
    Pause,
    Resume,
    Cancel,
    Retry,
    Priority,
    Approve,
    Deny,
    Review,
    Budget,
    Daemon,
    Mcp,
    Runtime,
    Plugins,
    RecoverRoot,
}

impl SlashCommand {
    fn control_spec(self) -> Option<CommandSpec> {
        let command = match self {
            Self::Agents => ControlCommand::Agents,
            Self::Agent => ControlCommand::Agent,
            Self::Events => ControlCommand::Events,
            Self::Diff => ControlCommand::Diff,
            Self::Mailbox => ControlCommand::Mailbox,
            Self::Message => ControlCommand::Message,
            Self::Pause => ControlCommand::Pause,
            Self::Resume => ControlCommand::Resume,
            Self::Cancel => ControlCommand::Cancel,
            Self::Retry => ControlCommand::Retry,
            Self::Priority => ControlCommand::Priority,
            Self::Approve => ControlCommand::Approve,
            Self::Deny => ControlCommand::Deny,
            Self::Review => ControlCommand::Review,
            Self::Budget => ControlCommand::Budget,
            Self::Daemon => ControlCommand::Daemon,
            Self::Mcp => ControlCommand::Mcp,
            Self::Help => ControlCommand::Help,
            Self::Quit
            | Self::Clear
            | Self::Model
            | Self::Effort
            | Self::Cost
            | Self::Compact
            | Self::Config
            | Self::Runtime
            | Self::Plugins
            | Self::RecoverRoot => {
                return None;
            }
        };
        Some(command.spec())
    }

    /// The serialized command name (without the leading `/`).
    pub fn name(&self) -> &'static str {
        if let Some(spec) = self.control_spec() {
            return spec.slash_name;
        }
        match self {
            SlashCommand::Quit => "quit",
            SlashCommand::Clear => "clear",
            SlashCommand::Model => "model",
            SlashCommand::Effort => "effort",
            SlashCommand::Cost => "cost",
            SlashCommand::Compact => "compact",
            SlashCommand::Config => "config",
            SlashCommand::Runtime => "runtime",
            SlashCommand::Plugins => "plugins",
            SlashCommand::RecoverRoot => "recover-root",
            // 其余变体都由上面的 `control_spec()` 提前返回。
            _ => unreachable!("control commands return their name from the catalog"),
        }
    }

    /// Short Chinese description shown in the popup.
    pub fn description(&self) -> &'static str {
        if let Some(spec) = self.control_spec() {
            return spec.description;
        }
        match self {
            SlashCommand::Quit => "退出程序",
            SlashCommand::Clear => "清空对话上下文",
            SlashCommand::Model => "查看模型清单 / 切换模型",
            SlashCommand::Effort => "查看或设置本会话的推理档（reasoning effort）",
            SlashCommand::Cost => "显示 token 使用量",
            SlashCommand::Compact => "压缩对话历史",
            SlashCommand::Config => "显示当前配置",
            SlashCommand::Runtime => "查看或设置子 Agent runtime 偏好",
            SlashCommand::Plugins => "列出已安装插件 / 查看与设置插件配置",
            SlashCommand::RecoverRoot => {
                "恢复卡死的前台会话根（先预览影响，再确认；不可用时可查看状态）"
            }
            // 其余变体都由上面的 `control_spec()` 提前返回。
            _ => unreachable!("control commands return their name from the catalog"),
        }
    }

    /// Required positional arguments, rendered consistently in help and completion.
    pub fn argument_usage(&self) -> Option<&'static str> {
        if let Some(spec) = self.control_spec() {
            return (!spec.usage.is_empty()).then_some(spec.usage);
        }
        match self {
            // `default` 是保留字（清空会话覆盖、回落清单 default_model），因此
            // 清单里同名的条目永远无法通过 `/model` 选中（见
            // `handle_model_command` 与未知名提示文案）。
            SlashCommand::Model => Some("[<name>|default]"),
            // `default` 同 `/model` 是保留字（清空本会话覆盖、回落条目默认）。
            SlashCommand::Effort => Some("[none|minimal|low|medium|high|xhigh|max|default]"),
            SlashCommand::Runtime => Some("[ask|always|never]"),
            SlashCommand::Plugins => Some("[<name> [set <key> <value>]]"),
            // Three separate steps, because each does strictly more than the last:
            // no argument or `status <id>` only reads, `prepare <id>` is the first
            // thing that stops work, and `confirm <id>` commits. A mistaken Enter
            // therefore cannot get past the read-only step.
            SlashCommand::RecoverRoot => Some(
                "[status <operation-id>|<operation-id>|prepare <operation-id>|confirm <operation-id>]",
            ),
            // 其余变体的 usage 由 `control_spec()` 提供；没有 usage 的返回 `None`。
            _ => None,
        }
    }

    /// Whether the command requires an argument.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn needs_arg(&self) -> bool {
        self.argument_usage().is_some()
    }

    /// All available commands, in popup display order.
    pub fn all() -> &'static [SlashCommand] {
        &[
            SlashCommand::Quit,
            SlashCommand::Clear,
            SlashCommand::Model,
            SlashCommand::Effort,
            SlashCommand::Cost,
            SlashCommand::Compact,
            SlashCommand::Config,
            SlashCommand::Help,
            SlashCommand::Agents,
            SlashCommand::Agent,
            SlashCommand::Events,
            SlashCommand::Diff,
            SlashCommand::Mailbox,
            SlashCommand::Message,
            SlashCommand::Pause,
            SlashCommand::Resume,
            SlashCommand::Cancel,
            SlashCommand::Retry,
            SlashCommand::Priority,
            SlashCommand::Approve,
            SlashCommand::Deny,
            SlashCommand::Review,
            SlashCommand::Budget,
            SlashCommand::Daemon,
            SlashCommand::Mcp,
            SlashCommand::Runtime,
            SlashCommand::Plugins,
            SlashCommand::RecoverRoot,
        ]
    }

    /// Commands shown in the popup and in the full `/help` listing.
    ///
    /// [`Self::all()`] stays the complete catalog so `from_name` keeps resolving
    /// commands whose backend does not exist yet; this view is what the user browses.
    /// Cached so the popup's per-keystroke calls do not rebuild (or leak) the list.
    pub fn completable() -> &'static [SlashCommand] {
        static COMPLETABLE: std::sync::OnceLock<Vec<SlashCommand>> = std::sync::OnceLock::new();
        COMPLETABLE.get_or_init(|| {
            SlashCommand::all()
                .iter()
                .copied()
                .filter(|cmd| cmd.unavailable_reason().is_none())
                .collect()
        })
    }

    /// Why a command has no backend yet, or `None` when it is usable.
    ///
    /// The reason is shown when the user explicitly types a hidden command: a bare
    /// "未知命令" would misreport a known-but-unwired command as a typo.
    pub fn unavailable_reason(&self) -> Option<&'static str> {
        match self {
            Self::Approve | Self::Deny => {
                Some("子 agent 验收由父 agent 真实合并后 daemon 自动观察，无交互式权限审批")
            }
            Self::Budget | Self::Priority => Some("daemon 尚未提供预算/优先级写接口"),
            _ => None,
        }
    }

    /// Look up a command by its name (without leading `/`).
    pub fn from_name(name: &str) -> Option<SlashCommand> {
        if name == "?" {
            return Some(Self::Help);
        }
        if name == "plugins" {
            return Some(Self::Plugins);
        }
        Self::all().iter().copied().find(|cmd| cmd.name() == name)
    }
}

/// Render the same command catalog used by slash completion and dispatch.
pub fn help_text(target: Option<&str>) -> String {
    match target
        .map(|name| name.trim_start_matches('/'))
        .filter(|name| !name.is_empty())
    {
        Some(name) => match SlashCommand::from_name(name) {
            Some(command) => format!(
                "/{}{}\n{}",
                command.name(),
                command
                    .argument_usage()
                    .map(|usage| format!(" {usage}"))
                    .unwrap_or_default(),
                command.description()
            ),
            None => match super::side::SIDE_HELP_ENTRIES.iter().find(|(usage, _)| {
                let canonical = usage.split_whitespace().next().unwrap_or_default();
                canonical == name
                        // `/btw` 是 `/side` 的别名：帮助要能查到它，但目录里
                        // 只保留规范名一行。
                        || (canonical == super::side::SIDE_COMMAND
                            && name == super::side::SIDE_ALIAS)
            }) {
                // 旁支命令不是 `SlashCommand`:它们在 `handle_key` 之前就被
                // 拦截了（否则运行中敲 `/side` 会先打断父会话）。但 `/help`
                // 必须能查到它们，否则用户只能靠猜。
                Some((usage, description)) => format!("/{usage}\n{description}"),
                None => format!("未知命令: /{name}\n使用 /help 查看可用命令。"),
            },
        },
        None => {
            let mut text = String::from("可用命令:\n");
            for command in SlashCommand::completable() {
                let usage = command
                    .argument_usage()
                    .map(|usage| format!(" {usage}"))
                    .unwrap_or_default();
                text.push_str(&format!(
                    "  /{}{} {}\n",
                    command.name(),
                    usage,
                    command.description()
                ));
            }
            text.push_str("\n旁问:\n");
            for (usage, description) in super::side::SIDE_HELP_ENTRIES {
                text.push_str(&format!("  /{usage} {description}\n"));
            }
            text
        }
    }
}

/// Handle to the process-wide live settings, as the TUI sees it.
///
/// Wrapped rather than used directly so [`TuiConfigSnapshot`] can keep deriving
/// `PartialEq`/`Eq` (the binding holds `Arc`s and deliberately has no `PartialEq`).
/// Equality compares *identity*: two snapshots carrying the same instance are
/// equal, and a test comparing a snapshot to itself still works.
#[derive(Clone)]
pub struct TuiLiveSettings {
    binding: yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding,
}

/// 三个可热更新的 TUI 行为开关，加标题开关。
///
/// 只有真正由渲染/事件循环消费的字段才在这里：`max_turns`、`compact.ratio`
/// 之类属于会话侧，与 TUI 本帧无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuiLiveControls {
    pub notifications: bool,
    pub notification_method: crate::tui::notify::Method,
    pub notification_condition: crate::tui::notify::Condition,
    pub terminal_title: bool,
}

impl TuiLiveControls {
    /// 从进程启动时的配置快照取初值。
    ///
    /// 用启动快照而不是立刻读盘：本函数在渲染第一帧之前调用，多一次文件
    /// 读取换不到任何东西，而 `RuntimeConfig` 已经用同一套归一化规则解析过
    /// 这三个值（非法值已按默认处理）。
    pub fn from_config(config: &TuiConfigSnapshot) -> Self {
        Self {
            notifications: config.notifications,
            notification_method: crate::tui::notify::Method::parse(&config.notification_method)
                .unwrap_or(crate::tui::notify::Method::Auto),
            notification_condition: crate::tui::notify::Condition::parse(
                &config.notification_condition,
            )
            .unwrap_or(crate::tui::notify::Condition::Unfocused),
            terminal_title: config.terminal_title,
        }
    }
}

impl TuiLiveSettings {
    pub fn new(binding: yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding) -> Self {
        Self { binding }
    }

    /// 从一份已解析的设置里读出 TUI 的三个行为开关。
    ///
    /// 与 `RuntimeConfig` 的解析规则逐字一致（字符串 `"false"` 才是关闭，
    /// 键缺失保持默认 true）：两处若不一致，保存后 `/config` 显示的值与
    /// 实际行为就会分叉。
    fn controls_from(&self, settings: &yi_agent_runtime::settings::Settings) -> TuiLiveControls {
        let effective = self.binding.sources.resolve(settings);
        let text = |key: &str| {
            effective
                .values
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        TuiLiveControls {
            notifications: !matches!(text("tui.notifications").as_deref(), Some("false")),
            notification_method: text("tui.notification_method")
                .and_then(|value| crate::tui::notify::Method::parse(&value))
                .unwrap_or(crate::tui::notify::Method::Auto),
            notification_condition: text("tui.notification_condition")
                .and_then(|value| crate::tui::notify::Condition::parse(&value))
                .unwrap_or(crate::tui::notify::Condition::Unfocused),
            terminal_title: !matches!(text("tui.terminal_title").as_deref(), Some("false")),
        }
    }

    /// 读一次最新设置并求出三个行为开关。
    ///
    /// 只在收到失效信号后调用（见 `LiveUiControls::poll`），不是每帧一次：
    /// 每帧读盘会把 30hz 的渲染变成 30 次文件 I/O。
    pub fn refresh_controls(&self) -> TuiLiveControls {
        // 优先用**已发布**快照：观察线程和本地提交都已经把触发这次唤醒的那份
        // 发布好了，再 `refresh()` 只会多读一次盘，并把「本次唤醒对应哪份内容」
        // 变成两次独立读取之间的猜测。只有从未发布过（观察器还没跑第一轮）时
        // 才真的读一次。
        match self.binding.live.published() {
            Some(snapshot) => self.controls_from(&snapshot.settings),
            None => self.controls_from(&self.binding.live.refresh().settings),
        }
    }

    /// 变化信号。本地保存与观察到的外部改动都会推进它。
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.binding.live.subscribe()
    }

    /// 起观察线程并拿到租约。宿主必须持有到事件循环结束。
    pub fn start_observing(
        &self,
    ) -> yi_agent_runtime::live_settings::observer::SettingsObservationLease {
        self.binding.live.start_observing()
    }

    /// Current effective values plus the origin of each one.
    ///
    /// Read at *render* time, never cached: `/config` must show what a save just
    /// changed, not what the process saw at start-up.
    fn view(&self) -> TuiLiveView {
        let snapshot = self.binding.live.refresh();
        let effective = self.binding.sources.resolve(&snapshot.settings);
        TuiLiveView {
            revision: snapshot.revision.clone(),
            degraded: snapshot.health.state
                == yi_agent_runtime::live_settings::HealthState::Degraded,
            health_code: snapshot.health.code.clone(),
            invalid_keys: snapshot.health.invalid_keys.clone(),
            max_turns: effective.values.get("agent.max_turns").cloned(),
            compact_ratio: effective.values.get("compact.ratio").cloned(),
            notifications: effective.values.get("tui.notifications").cloned(),
            terminal_title: effective.values.get("tui.terminal_title").cloned(),
            sources: effective
                .sources
                .iter()
                .map(|(key, source)| (key.clone(), source_name(*source)))
                .collect(),
        }
    }
}

impl std::fmt::Debug for TuiLiveSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只报"有绑定"，绝不显示取值（提示词可能在里面）。
        formatter.write_str("TuiLiveSettings")
    }
}

impl PartialEq for TuiLiveSettings {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.binding.live, &other.binding.live)
    }
}

impl Eq for TuiLiveSettings {}

/// 一次渲染时读到的动态快照。
struct TuiLiveView {
    revision: String,
    degraded: bool,
    health_code: Option<String>,
    invalid_keys: Vec<String>,
    max_turns: Option<serde_json::Value>,
    compact_ratio: Option<serde_json::Value>,
    notifications: Option<serde_json::Value>,
    terminal_title: Option<serde_json::Value>,
    sources: std::collections::BTreeMap<String, &'static str>,
}

fn source_name(source: yi_agent_runtime::live_settings::sources::ValueSource) -> &'static str {
    use yi_agent_runtime::live_settings::sources::ValueSource;
    match source {
        ValueSource::Override => "override",
        ValueSource::Environment => "environment",
        ValueSource::Settings => "settings",
        ValueSource::Default => "default",
    }
}

impl TuiLiveView {
    /// 一个字段的 `值（来源）`。值缺失时只报来源，不臆造默认值。
    fn item(&self, key: &str, value: Option<&serde_json::Value>) -> String {
        let source = self.sources.get(key).copied().unwrap_or("default");
        match value {
            Some(serde_json::Value::Null) | None => format!("默认（{source}）"),
            Some(value) => match value {
                serde_json::Value::String(text) => format!("{text}（{source}）"),
                other => format!("{other}（{source}）"),
            },
        }
    }
}

/// Read-only, secret-free view of the running session's configuration.
///
/// Assembled once in `main.rs` from `RuntimeConfig` / `AgentConfig` / the MCP
/// manager / the persisted runtime preference. The model is **not** stored here:
/// `/model` can change it mid-session, so `render` takes the live value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiConfigSnapshot {
    pub provider: String,
    /// 本进程的模型整包是否注入（`RuntimeConfig::model_injected`）。
    ///
    /// `/effort` 需要它来选择生效条目：注入存在时清单默认不生效，请求体不带
    /// `reasoning_effort`，上报也必须同口径（否则「上报 != 实际发出」）。
    /// 必须**如实**取自真实配置，绝不能拿清单反推。
    pub model_injected: bool,
    pub workdir: std::path::PathBuf,
    pub sandbox: String,
    pub yolo: bool,
    pub max_turns: u32,
    pub compact_threshold: u32,
    pub mcp_master: bool,
    pub runtime_preference: String,
    pub runtime_preference_path: std::path::PathBuf,
    /// 是否发送终端通知。
    pub notifications: bool,
    /// 配置里的协议值（`auto` / `osc9` / `bel`）。
    pub notification_method: String,
    /// 配置里的触发条件（`unfocused` / `always`）。
    pub notification_condition: String,
    /// `auto` 解析后的实际后端。
    pub effective_backend: String,
    /// 是否写终端标签标题（`tui.terminal_title`，默认 true）。
    pub terminal_title: bool,
    /// 标题状态：`on` / `off` / `disabled (multiplexer)`。
    ///
    /// `disabled (write failed)` 属运行期状态，而这是启动快照，看不到它。
    pub title_status: String,
    /// 终端拒绝启用焦点事件时为真；此时 `unfocused` 实际退化为 `always`。
    pub focus_degraded: bool,
    /// 进程级运行期设置句柄。`None` 表示这是个不消费热更新的宿主（旧行为）。
    pub live: Option<TuiLiveSettings>,
}

impl TuiConfigSnapshot {
    /// Render the config for the transcript. `model` is the live model, so a
    /// `/model` switch shows up here without re-reading anything.
    pub fn render(&self, model: &str) -> String {
        format!(
            "当前配置:\n  provider: {}\n  model: {}\n  workdir: {}\n  sandbox: {}\n  yolo: {}\n  max_turns: {}\n  compact_threshold: {}\n  mcp master: {}\n  subagent runtime: {}（{}）",
            self.provider,
            model,
            self.workdir.display(),
            self.sandbox,
            if self.yolo { "on" } else { "off" },
            self.max_turns,
            self.compact_threshold,
            if self.mcp_master { "on" } else { "off" },
            self.runtime_preference,
            self.runtime_preference_path.display(),
        ) + &format!(
            "\n\n  terminal notify: {}（method={} → {}, condition={}）",
            if self.notifications { "on" } else { "off" },
            self.notification_method,
            if self.effective_backend.is_empty() {
                self.notification_method.as_str()
            } else {
                self.effective_backend.as_str()
            },
            self.notification_condition,
        ) + &format!("\n  terminal title: {}", self.title_status)
            + &self.render_live()
    }

    /// 运行期设置的当前有效值与来源。
    ///
    /// 只有持有绑定时才出现：没有绑定的宿主没有热更新，多印一段"当前值"会
    /// 让用户以为保存会立即改变它。值在渲染时读，因此保存后再次 `/config`
    /// 立刻看到新值。**不**打印设置文件原文（提示词属于用户内容）。
    fn render_live(&self) -> String {
        let Some(handle) = &self.live else {
            return String::new();
        };
        let view = handle.view();
        let revision = view
            .revision
            .get(..12)
            .unwrap_or(view.revision.as_str())
            .to_string();
        let mut text = format!(
            "\n\n  运行期设置（revision {revision}）:\n    适用: 下一个安全执行点，无需重启\n    max_turns: {}\n    compact ratio: {}\n    通知: {}\n    标题: {}",
            view.item("agent.max_turns", view.max_turns.as_ref()),
            view.item("compact.ratio", view.compact_ratio.as_ref()),
            view.item("tui.notifications", view.notifications.as_ref()),
            view.item("tui.terminal_title", view.terminal_title.as_ref()),
        );
        if view.degraded {
            // 说清"当前用的是回退值"，并给出要修的键名——不给值。
            let keys = if view.invalid_keys.is_empty() {
                "未知".to_string()
            } else {
                view.invalid_keys.join(", ")
            };
            text.push_str(&format!(
                "\n    配置读取异常（{}）: 当前使用最后有效值/启动回退值，新设置尚不能保证生效；需修复: {keys}",
                view.health_code.as_deref().unwrap_or("unknown")
            ));
        }
        text
    }
}

/// A parsed `/runtime` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAction {
    /// Show the current preference and its source.
    Status,
    /// Persist a new preference.
    Set(RuntimePreference),
}

/// Parse `/runtime` arguments. Returns a user-facing usage error on bad input.
pub fn parse_runtime_args(args: &str) -> Result<RuntimeAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(RuntimeAction::Status),
        ["ask"] => Ok(RuntimeAction::Set(RuntimePreference::Ask)),
        ["always"] => Ok(RuntimeAction::Set(RuntimePreference::Always)),
        ["never"] => Ok(RuntimeAction::Set(RuntimePreference::Never)),
        _ => Err("用法: /runtime [ask|always|never]".into()),
    }
}

/// A parsed `/effort` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortAction {
    /// Show the current effective effort and where it comes from.
    Status,
    /// Clear the session override and fall back to the catalog entry's default.
    Clear,
    /// Set the session override to one of the seven tiers.
    Set(String),
}

/// Parse `/effort` arguments. Returns a user-facing usage error on bad input.
///
/// `default` is a reserved word (clear the override), mirroring `/model`; it is
/// deliberately not spellable as a tier.
pub fn parse_effort_args(args: &str) -> Result<EffortAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(EffortAction::Status),
        ["default"] => Ok(EffortAction::Clear),
        [tier] if REASONING_EFFORT_TIERS.contains(tier) => Ok(EffortAction::Set((*tier).into())),
        _ => Err(format!(
            "用法: /effort [{}|default]",
            REASONING_EFFORT_TIERS.join("|")
        )),
    }
}

/// 与 [`parse_effort_args`] 相同，但额外按模型能力集合校验档位。
///
/// `supported = None` 表示能力未知（没探测过 / 探测失败）：沿用七档白名单，
/// 与探测功能上线前的行为一致。只有**有可信结论**时才收窄——把「还没测」当成
/// 「一档都不支持」会把用户挡在一个本来可用的功能外面。
pub fn parse_effort_args_with_caps(
    args: &str,
    supported: Option<&[String]>,
) -> Result<EffortAction, String> {
    let action = parse_effort_args(args)?;
    if let (EffortAction::Set(tier), Some(list)) = (&action, supported)
        && !list.iter().any(|s| s == tier)
    {
        return Err(format!(
            "该模型不支持 {tier}（支持：{}）",
            if list.is_empty() {
                "无，仅 `default`".to_string()
            } else {
                list.join(" | ")
            }
        ));
    }
    Ok(action)
}

/// A parsed `/mcp` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAction {
    /// List configured servers and their effective on/off state.
    Status,
    /// Re-read `.yi-agent/mcp.json` from disk and refresh the tool set.
    Reload,
    /// Toggle the session-wide master switch.
    Master(bool),
    /// Toggle one server.
    Server { name: String, on: bool },
}

/// Parse `/mcp` arguments. Returns a user-facing usage error on bad input.
pub fn parse_mcp_args(args: &str) -> Result<McpAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(McpAction::Status),
        ["reload"] => Ok(McpAction::Reload),
        ["on"] => Ok(McpAction::Master(true)),
        ["off"] => Ok(McpAction::Master(false)),
        ["enable", name] => Ok(McpAction::Server {
            name: (*name).into(),
            on: true,
        }),
        ["disable", name] => Ok(McpAction::Server {
            name: (*name).into(),
            on: false,
        }),
        _ => Err("用法: /mcp [on|off|reload|enable <server>|disable <server>|status]".into()),
    }
}

/// A parsed `/daemon` action. `start` is deliberately absent: the TUI embeds
/// its own daemon, so starting a detached one from here would be ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAction {
    Status,
    Stop,
}

/// Parse `/daemon` arguments. Returns a user-facing usage error on bad input.
pub fn parse_daemon_args(args: &str) -> Result<DaemonAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(DaemonAction::Status),
        ["stop"] => Ok(DaemonAction::Stop),
        _ => Err("用法: /daemon [status|stop]".into()),
    }
}

/// Render `/mcp status` output. Pure so it is unit-testable without a manager.
pub fn render_mcp_status(master: bool, servers: &[(String, bool)]) -> String {
    if servers.is_empty() {
        return "未配置 MCP server".to_string();
    }
    let mut out = format!("MCP master: {}\n", if master { "on" } else { "off" });
    for (name, on) in servers {
        out.push_str(&format!("  {name}: {}\n", if *on { "on" } else { "off" }));
    }
    out
}

/// Maximum popup height in rows, including borders. Shared by the layout (which
/// reserves the rows) and the widget (which fills them) so they cannot drift.
const POPUP_MAX_ROWS: usize = 10;
/// Rows `compute_layout` is expected to keep outside the popup on a comfortable
/// terminal: history minimum (3), status bar (1), blank gap (1), input (1).
/// Queued messages and a wrapping input can claim more, so this is a floor for
/// the height estimate, not a guarantee. A squeezed layout hands the popup a
/// smaller area, which is why the draw path re-clamps rather than trusting it.
const MIN_NON_POPUP_ROWS: usize = 6;

/// One completion candidate.
///
/// A `Completion` is not a `SlashCommand`: the side view's popup offers
/// `/back` and `/side-close`, which are routed by `classify_conversation_input`
/// and have no `SlashCommand` variant. Keeping the popup on this small type is
/// what lets the **candidate list and the executor** be the same list — the
/// spec's "不能显示候选却由另一套输入分支忽略" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    name: &'static str,
    description: &'static str,
}

impl Completion {
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Self { name, description }
    }

    /// Command name **without** the leading slash.
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn description(&self) -> &'static str {
        self.description
    }

    /// The text Tab writes into the visible input buffer.
    pub fn insertion(&self) -> String {
        format!("/{}", self.name)
    }
}

/// Which view a [`CommandPopup`] belongs to.
///
/// The two views offer different commands because they execute different
/// handlers: the side view is read-only and can only navigate, while the main
/// view drives the parent session. Sharing one candidate list would mean the
/// popup offers commands the executor must then refuse — exactly the
/// "candidates the input branch ignores" failure the spec forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupScope {
    Main,
    Side,
}

impl PopupScope {
    /// Completions offered in this view, in display order.
    pub fn completions(self) -> &'static [Completion] {
        match self {
            Self::Main => to_completions(SlashCommand::completable()),
            Self::Side => SIDE_COMPLETIONS,
        }
    }
}

fn to_completions(commands: &'static [SlashCommand]) -> &'static [Completion] {
    // Built once per catalog: the popup filters on every keystroke.
    static MAIN: std::sync::OnceLock<Vec<Completion>> = std::sync::OnceLock::new();
    MAIN.get_or_init(|| {
        commands
            .iter()
            .map(|cmd| Completion::new(cmd.name(), cmd.description()))
            .collect()
    })
}

/// Side-view completions.
///
/// Exactly the commands `classify_conversation_input` accepts inside a side:
/// navigation (`/back`, `/side-close`), the three read-only local commands, and
/// `/quit` (application exit stays reachable from every view — spec §6.2).
/// `/btw` and `/side` are deliberately absent: a side cannot nest.
pub const SIDE_COMPLETIONS: &[Completion] = &[
    Completion::new("back", "返回主会话，保留旁问及其在途轮次"),
    Completion::new("side-close", "关闭当前旁问（取消在途轮次）"),
    Completion::new("help", "旁支里可用的命令与只读工具"),
    Completion::new("config", "旁支继承的固定配置（只读）"),
    Completion::new("cost", "只统计旁支自身的用量"),
    Completion::new("quit", "退出程序"),
    // `/q` 与 `/quit` 等价（既有主视图按键分支 `matches!(name, "quit" | "q")`）：
    // 候选里不列它，用户就只能靠记忆，而输入 `/q` 又被当作普通问题发给模型。
    Completion::new("q", "退出程序（/quit 的简写）"),
];

/// State for the slash command popup shown above the input area.
pub struct CommandPopup {
    filtered: Vec<Completion>,
    selected: usize,
    /// Index of the first command drawn in the visible window. The filtered
    /// list can be taller than the popup, so this is what keeps the highlighted
    /// row on screen.
    offset: usize,
    last_filter: String,
    /// The view this popup belongs to; filters candidates and is checked by the
    /// Enter handler before executing anything.
    scope: PopupScope,
}

impl CommandPopup {
    /// Create a new popup with every completable command visible.
    pub fn new() -> Self {
        Self::with_scope(PopupScope::Main)
    }

    /// Popup for a given view. The scope decides **which commands exist** in
    /// this view, so the visible candidates can never disagree with what the
    /// Enter handler is willing to execute.
    pub fn with_scope(scope: PopupScope) -> Self {
        Self {
            filtered: scope.completions().to_vec(),
            selected: 0,
            offset: 0,
            last_filter: String::new(),
            scope,
        }
    }

    /// Which view this popup was built for.
    pub fn scope(&self) -> PopupScope {
        self.scope
    }

    /// Filter the command list by a prefix string (the text after `/`).
    /// Resets the selection to the first item only if the filter changed.
    pub fn filter(&mut self, text: &str) {
        let text = text.trim();
        if text == self.last_filter {
            return; // No change, preserve selection
        }
        self.last_filter = text.to_string();
        if text.is_empty() {
            self.filtered = self.scope.completions().to_vec();
        } else {
            self.filtered = self
                .scope
                .completions()
                .iter()
                .copied()
                .filter(|cmd| cmd.name().starts_with(text))
                .collect();
        }
        self.selected = 0;
        self.offset = 0;
    }

    /// The filtered list of commands. Production draws through [`Self::visible`];
    /// this stays for assertions about the underlying list.
    #[cfg(test)]
    pub fn filtered(&self) -> &[Completion] {
        &self.filtered
    }

    /// Move the selection up by one, wrapping to the bottom.
    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.filtered.len() - 1;
        } else {
            self.selected -= 1;
        }
        self.scroll_to_selection();
    }

    /// Move the selection down by one, wrapping to the top.
    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
        self.scroll_to_selection();
    }

    /// The currently selected completion, if any.
    pub fn selected(&self) -> Option<Completion> {
        self.filtered.get(self.selected).copied()
    }

    /// The index of the currently selected item.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Index of the first command actually drawn, clamped so the highlight sits
    /// inside a window of `height` rows even when the layout squeezes the popup
    /// shorter than [`Self::height`] predicted. [`Self::window_start`] reports
    /// this value, so the widget's highlight index and the drawn rows stay in
    /// the same coordinate space.
    pub fn window_start(&self, height: usize) -> usize {
        let window = self.effective_window(height);
        self.clamped_start(window)
    }

    /// The commands to draw, at most `height` of them: the effective window,
    /// which always contains the selection (unless the list is empty).
    pub fn visible(&self, height: usize) -> &[Completion] {
        let start = self.clamped_start(self.effective_window(height));
        let end = (start + height).min(self.filtered.len());
        &self.filtered[start..end]
    }

    /// Rows the popup reserves for commands while handling keys -- `height` for
    /// a comfortable terminal. The draw path sizes from the live popup area and
    /// re-clamps, so a squeezed layout can never hide the selection either.
    fn window_height(&self) -> usize {
        POPUP_MAX_ROWS.saturating_sub(2).max(1)
    }

    /// Fold the selection into a `height`-row window without narrowing below
    /// what the cursor needs, so the highlight is always drawn.
    fn effective_window(&self, height: usize) -> usize {
        height.max(1).min(self.filtered.len().max(1))
    }

    /// Window start that keeps `selected` visible within `window` rows, and
    /// stays at 0 when the list is short enough to fit.
    fn clamped_start(&self, window: usize) -> usize {
        let start = if self.filtered.len() <= window {
            0
        } else if self.selected < self.offset {
            self.selected
        } else if self.selected >= self.offset + window {
            self.selected + 1 - window
        } else {
            self.offset
        };
        start.min(self.filtered.len().saturating_sub(window))
    }

    /// Slide the window so `selected` is inside it, touching only what is
    /// needed: at most one row of scrolling per move keeps the list steady
    /// rather than jumping.
    fn scroll_to_selection(&mut self) {
        let height = self.window_height();
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
    }

    /// Rows the popup occupies (borders included) for a terminal `area_height`
    /// tall: tall enough for every command, but capped so a long catalog can
    /// never crowd out history, status, or the input. An empty filter keeps the
    /// minimal border-only box rather than reserving the full cap.
    pub fn height(&self, area_height: u16) -> usize {
        let available = (area_height as usize).saturating_sub(MIN_NON_POPUP_ROWS);
        (self.filtered.len() + 2)
            .min(POPUP_MAX_ROWS)
            .min(available)
            .max(1)
    }
}

impl Default for CommandPopup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn filter_empty_shows_all() {
        let popup = CommandPopup::new();
        assert_eq!(popup.filtered().len(), SlashCommand::completable().len());
    }

    #[test]
    fn popup_never_lists_hidden_commands() {
        let mut popup = CommandPopup::new();
        popup.filter("a"); // would otherwise match approve
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name()).collect();
        assert!(!names.contains(&"approve"));
    }

    #[test]
    fn filter_prefix_matches() {
        let mut popup = CommandPopup::new();
        popup.filter("cl");
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["clear"]);
    }

    #[test]
    fn filter_no_match() {
        let mut popup = CommandPopup::new();
        popup.filter("xyz");
        assert!(popup.filtered().is_empty());
        assert!(popup.selected().is_none());
    }

    #[test]
    fn filter_resets_selection() {
        let mut popup = CommandPopup::new();
        popup.move_down();
        popup.move_down();
        assert!(popup.selected > 0);
        popup.filter("c");
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn move_down_wraps() {
        let mut popup = CommandPopup::new();
        let len = popup.filtered().len();
        for _ in 0..len {
            popup.move_down();
        }
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn move_up_wraps() {
        let mut popup = CommandPopup::new();
        let len = popup.filtered().len();
        popup.move_up();
        assert_eq!(popup.selected, len - 1);
    }

    #[test]
    fn move_up_down_on_empty_filtered() {
        let mut popup = CommandPopup::new();
        popup.filter("xyz");
        popup.move_up();
        popup.move_down();
        assert!(popup.selected().is_none());
    }

    #[test]
    fn visible_window_contains_the_selection_after_scrolling_past_the_cap() {
        let mut popup = CommandPopup::new();
        let total = popup.filtered().len();
        let window = POPUP_MAX_ROWS - 2;
        assert!(total > window, "the catalog must be long enough to scroll");
        for _ in 0..total {
            popup.move_down();
            let drawn = popup.visible(window);
            assert!(
                drawn.contains(&popup.selected().unwrap()),
                "selection escaped the visible window at index {}",
                popup.selected_index()
            );
        }
    }

    /// A squeezed layout hands the popup fewer rows than [`CommandPopup::height`]
    /// predicted. The drawn window must still contain the highlight.
    #[test]
    fn drawn_window_clamps_the_selection_when_the_popup_is_squeezed() {
        let mut popup = CommandPopup::new();
        let squeezed = 6;
        for _ in 0..12 {
            popup.move_down();
        }
        assert!(popup.selected_index() >= squeezed);
        assert!(
            popup.visible(squeezed).contains(&popup.selected().unwrap()),
            "highlight escaped the squeezed window"
        );
        assert_eq!(
            popup.window_start(squeezed) + popup.visible(squeezed).len(),
            popup.selected_index() + 1,
            "the highlight must be the last drawn row"
        );
    }

    #[test]
    fn scrolling_past_the_cap_advances_the_window() {
        let mut popup = CommandPopup::new();
        let window = POPUP_MAX_ROWS - 2;
        // The window holds indices `0..window`; the selection stays inside it
        // until it steps one past the last visible row.
        for _ in 0..window - 1 {
            popup.move_down();
        }
        assert_eq!(
            popup.window_start(window),
            0,
            "window must not move until it fills"
        );
        popup.move_down();
        assert_eq!(
            popup.window_start(window),
            1,
            "window follows the selection down"
        );
    }

    #[test]
    fn move_up_wraps_and_keeps_the_last_command_visible() {
        let mut popup = CommandPopup::new();
        let window = POPUP_MAX_ROWS - 2;
        popup.move_up();
        assert_eq!(popup.selected_index(), popup.filtered().len() - 1);
        assert!(popup.visible(window).contains(&popup.selected().unwrap()));
    }

    #[test]
    fn filtering_resets_the_window_to_the_top() {
        let mut popup = CommandPopup::new();
        let window = POPUP_MAX_ROWS - 2;
        for _ in 0..12 {
            popup.move_down();
        }
        assert!(popup.window_start(window) > 0);
        popup.filter("c");
        assert_eq!(popup.window_start(window), 0);
    }

    #[test]
    fn height_lists_every_command_until_the_cap() {
        let popup = CommandPopup::new();
        assert_eq!(
            popup.height(24),
            10,
            "the full catalog is capped at 10 rows"
        );
        assert_eq!(popup.height(50), 10);
    }

    #[test]
    fn height_reserves_rows_for_history_on_a_short_terminal() {
        let popup = CommandPopup::new();
        // 8 rows - 6 reserved = 2 available, even though the catalog is longer.
        assert_eq!(popup.height(8), 2);
        assert_eq!(popup.height(0), 1);
    }

    #[test]
    fn height_shrinks_to_fit_a_filtered_list() {
        let mut popup = CommandPopup::new();
        popup.filter("cl");
        assert_eq!(popup.filtered().len(), 1);
        assert_eq!(popup.height(24), 3, "one command plus borders");
    }

    #[test]
    fn height_stays_minimal_when_nothing_matches() {
        let mut popup = CommandPopup::new();
        popup.filter("zzz");
        assert_eq!(popup.filtered().len(), 0);
        assert_eq!(popup.height(24), 2, "borders only: no blank rows budget");
    }

    #[test]
    fn visible_returns_the_whole_list_when_it_fits() {
        let mut popup = CommandPopup::new();
        popup.filter("r");
        let matched = popup.filtered().len();
        assert!(matched > 0 && matched < 8, "pick a filter that fits");
        assert_eq!(popup.visible(8).len(), matched);
    }

    #[test]
    fn selected_returns_correct_command() {
        let mut popup = CommandPopup::new();
        popup.move_down();
        assert_eq!(popup.selected().map(|c| c.name()), Some("clear"));
        popup.move_down();
        assert_eq!(popup.selected().map(|c| c.name()), Some("model"));
    }

    #[test]
    fn foreground_root_lifecycle_recover_root_is_a_host_command() {
        // Reaching recovery from the TUI must go through this host's own
        // authorization, which is why the command is host-local: a supervisor
        // control command would carry a task id, and a task id is exactly the thing
        // that must not be enough to stop a conversation.
        assert_eq!(
            SlashCommand::from_name("recover-root"),
            Some(SlashCommand::RecoverRoot)
        );
        assert!(
            SlashCommand::RecoverRoot.control_spec().is_none(),
            "recovery is not a supervisor control command"
        );
        let usage = SlashCommand::RecoverRoot
            .argument_usage()
            .expect("recover-root takes an operation id");
        assert!(
            usage.contains("status"),
            "the usage must offer the read-only form, not only the mutating one"
        );
        // Each step stops strictly more than the previous one, so the usage has to
        // name all three: a user who only saw `prepare` would not know `confirm`
        // exists, and one who only saw `confirm` would think a token was an argument.
        assert!(
            usage.contains("prepare"),
            "the usage must name the stop step"
        );
        assert!(
            usage.contains("confirm"),
            "the usage must name the commit step"
        );
        // Wired now: the driver holds the lease and the fence, so the command is
        // usable and therefore offered.
        assert!(
            SlashCommand::RecoverRoot.unavailable_reason().is_none(),
            "a wired command must not be reported as unusable"
        );
        assert!(
            SlashCommand::completable().contains(&SlashCommand::RecoverRoot),
            "completion must offer a command that has a backend"
        );
    }

    #[test]
    fn foreground_root_lifecycle_recover_root_help_explains_the_preview() {
        let help = help_text(Some("recover-root"));
        assert!(help.contains("/recover-root"));
        assert!(
            help.contains("预览"),
            "help must say the impact is shown before confirming"
        );
        // The former `/agents` listing keeps working untouched.
        assert!(help_text(Some("agents")).contains("/agents"));
    }

    #[test]
    fn from_name_finds_command() {
        assert_eq!(SlashCommand::from_name("quit"), Some(SlashCommand::Quit));
        assert_eq!(SlashCommand::from_name("clear"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::from_name("xyz"), None);
    }

    #[test]
    fn help_alias_and_contextual_help_use_the_command_catalog() {
        assert_eq!(SlashCommand::from_name("?"), Some(SlashCommand::Help));
        assert!(help_text(Some("agents")).contains("/agents"));
        assert!(help_text(Some("missing")).contains("未知命令"));
    }

    #[test]
    fn task_controls_publish_usage_in_help_and_metadata() {
        for command in [
            SlashCommand::Pause,
            SlashCommand::Resume,
            SlashCommand::Retry,
        ] {
            assert_eq!(command.argument_usage(), Some("<task-id>"));
            assert!(command.needs_arg());
            assert!(
                help_text(Some(command.name())).contains(&format!("/{} <task-id>", command.name()))
            );
        }
    }

    #[test]
    fn executable_commands_publish_their_argument_signatures() {
        for (command, usage) in [
            (SlashCommand::Agent, "<task-id>"),
            (SlashCommand::Message, "<task-id> <text>"),
            (SlashCommand::Cancel, "<task-id> [--recursive]"),
            // `default` 是保留字:清单里同名的条目无法被选中,用法文案据此列出。
            (SlashCommand::Model, "[<name>|default]"),
        ] {
            assert_eq!(command.argument_usage(), Some(usage));
            assert!(command.needs_arg());
            assert!(
                help_text(Some(command.name())).contains(&format!("/{} {usage}", command.name()))
            );
        }
    }

    #[test]
    fn needs_arg_includes_commands_with_usage_metadata() {
        for cmd in SlashCommand::all() {
            assert_eq!(cmd.needs_arg(), cmd.argument_usage().is_some());
        }
    }

    #[test]
    fn all_commands_have_unique_names() {
        let names: Vec<&str> = SlashCommand::all().iter().map(|c| c.name()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "duplicate command names");
    }

    #[test]
    fn mcp_command_is_registered_with_usage() {
        assert_eq!(SlashCommand::from_name("mcp"), Some(SlashCommand::Mcp));
        assert_eq!(
            SlashCommand::Mcp.argument_usage(),
            Some("[on|off|reload|enable <server>|disable <server>|status]")
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Mcp));
    }

    #[test]
    fn runtime_command_is_registered_with_usage() {
        assert_eq!(
            SlashCommand::from_name("runtime"),
            Some(SlashCommand::Runtime)
        );
        assert_eq!(
            SlashCommand::Runtime.argument_usage(),
            Some("[ask|always|never]")
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Runtime));
    }

    /// `/effort` 接受七档、`default`、空参（=status），拒绝白名单外的值。
    ///
    /// 空参与 `default` 分属两个动作（看状态 vs 清覆盖），绝不能混：否则
    /// `/effort default` 会静默变成「打印状态」，用户的清覆盖意图就丢了。
    #[test]
    fn parse_effort_args_accepts_every_tier_clear_and_status() {
        assert_eq!(parse_effort_args(""), Ok(EffortAction::Status));
        assert_eq!(parse_effort_args("status"), Ok(EffortAction::Status));
        assert_eq!(parse_effort_args("  "), Ok(EffortAction::Status));
        assert_eq!(parse_effort_args("default"), Ok(EffortAction::Clear));
        for tier in REASONING_EFFORT_TIERS {
            assert_eq!(
                parse_effort_args(tier),
                Ok(EffortAction::Set(tier.to_string())),
                "tier {tier} must parse"
            );
        }
    }

    #[test]
    fn effort_rejects_a_tier_the_model_does_not_support() {
        let supported = vec!["low".to_string(), "high".to_string()];
        let err = parse_effort_args_with_caps("max", Some(&supported)).unwrap_err();
        assert!(
            err.contains("low") && err.contains("high"),
            "error must list the supported tiers: {err}"
        );
        assert_eq!(
            parse_effort_args_with_caps("high", Some(&supported)),
            Ok(EffortAction::Set("high".into()))
        );
        // 能力未知时行为不变。
        assert_eq!(
            parse_effort_args_with_caps("max", None),
            Ok(EffortAction::Set("max".into()))
        );
    }

    #[test]
    fn effort_clear_and_status_ignore_the_capability_list() {
        let none: Vec<String> = Vec::new();
        assert_eq!(
            parse_effort_args_with_caps("default", Some(&none)),
            Ok(EffortAction::Clear),
            "clearing an override must stay possible even when no tier works"
        );
        assert_eq!(
            parse_effort_args_with_caps("", Some(&none)),
            Ok(EffortAction::Status)
        );
    }

    #[test]
    fn effort_rejects_every_tier_when_the_model_supports_none() {
        let none: Vec<String> = Vec::new();
        let err = parse_effort_args_with_caps("low", Some(&none)).unwrap_err();
        assert!(err.contains("不支持"), "got: {err}");
        assert!(
            err.contains("default"),
            "must point at the escape hatch: {err}"
        );
    }

    #[test]
    fn parse_effort_args_rejects_unknown_tier_and_extra_tokens() {
        let usage = format!(
            "用法: /effort [{}|default]",
            REASONING_EFFORT_TIERS.join("|")
        );
        assert_eq!(parse_effort_args("bogus"), Err(usage.clone()));
        assert_eq!(parse_effort_args("high max"), Err(usage.clone()));
        assert_eq!(parse_effort_args("HIGH"), Err(usage));
    }

    #[test]
    fn effort_command_is_registered_with_usage_and_help() {
        assert_eq!(
            SlashCommand::from_name("effort"),
            Some(SlashCommand::Effort)
        );
        assert_eq!(SlashCommand::Effort.name(), "effort");
        assert_eq!(
            SlashCommand::Effort.argument_usage(),
            Some("[none|minimal|low|medium|high|xhigh|max|default]")
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Effort));
        assert!(help_text(None).contains("/effort"));
        assert!(help_text(Some("effort")).contains("/effort"));
    }

    #[test]
    fn parse_runtime_args_accepts_status_and_every_state() {
        assert_eq!(parse_runtime_args(""), Ok(RuntimeAction::Status));
        assert_eq!(parse_runtime_args("status"), Ok(RuntimeAction::Status));
        assert_eq!(
            parse_runtime_args("ask"),
            Ok(RuntimeAction::Set(RuntimePreference::Ask))
        );
        assert_eq!(
            parse_runtime_args("always"),
            Ok(RuntimeAction::Set(RuntimePreference::Always))
        );
        assert_eq!(
            parse_runtime_args("never"),
            Ok(RuntimeAction::Set(RuntimePreference::Never))
        );
    }

    #[test]
    fn parse_runtime_args_rejects_unknown_input() {
        assert_eq!(
            parse_runtime_args("sometimes"),
            Err("用法: /runtime [ask|always|never]".into())
        );
        assert_eq!(
            parse_runtime_args("always never"),
            Err("用法: /runtime [ask|always|never]".into())
        );
    }

    #[test]
    fn parse_mcp_args_empty_and_status_mean_status() {
        assert_eq!(parse_mcp_args(""), Ok(McpAction::Status));
        assert_eq!(parse_mcp_args("status"), Ok(McpAction::Status));
    }

    #[test]
    fn parse_mcp_args_reload_is_its_own_action() {
        assert_eq!(parse_mcp_args("reload"), Ok(McpAction::Reload));
        // 不能把 `reload` 误当成 server 名。
        assert_eq!(
            parse_mcp_args("enable reload"),
            Ok(McpAction::Server {
                name: "reload".into(),
                on: true,
            })
        );
    }

    #[test]
    fn parse_mcp_args_on_and_off_toggle_master() {
        assert_eq!(parse_mcp_args("on"), Ok(McpAction::Master(true)));
        assert_eq!(parse_mcp_args("off"), Ok(McpAction::Master(false)));
    }

    #[test]
    fn parse_mcp_args_enable_disable_toggle_one_server() {
        assert_eq!(
            parse_mcp_args("enable fs"),
            Ok(McpAction::Server {
                name: "fs".into(),
                on: true
            })
        );
        assert_eq!(
            parse_mcp_args("disable fs"),
            Ok(McpAction::Server {
                name: "fs".into(),
                on: false
            })
        );
    }

    #[test]
    fn parse_mcp_args_rejects_bad_input_with_usage_message() {
        assert_eq!(
            parse_mcp_args("bogus"),
            Err("用法: /mcp [on|off|reload|enable <server>|disable <server>|status]".into())
        );
        assert_eq!(
            parse_mcp_args("enable"),
            Err("用法: /mcp [on|off|reload|enable <server>|disable <server>|status]".into())
        );
    }

    #[test]
    fn render_mcp_status_without_servers_says_unconfigured() {
        assert_eq!(render_mcp_status(true, &[]), "未配置 MCP server");
    }

    #[test]
    fn parse_daemon_args_defaults_to_status() {
        assert_eq!(parse_daemon_args(""), Ok(DaemonAction::Status));
        assert_eq!(parse_daemon_args("status"), Ok(DaemonAction::Status));
        assert_eq!(parse_daemon_args("stop"), Ok(DaemonAction::Stop));
    }

    #[test]
    fn parse_daemon_args_rejects_start_and_unknown() {
        assert!(parse_daemon_args("start").is_err());
        assert!(parse_daemon_args("bogus").is_err());
    }

    #[test]
    fn daemon_usage_advertises_only_status_and_stop() {
        let usage = SlashCommand::Daemon.argument_usage().unwrap();
        assert_eq!(usage, "[status|stop]");
    }

    #[test]
    fn render_mcp_status_lists_master_and_each_server() {
        assert_eq!(
            render_mcp_status(true, &[("fs".into(), true), ("gh".into(), false)]),
            "MCP master: on\n  fs: on\n  gh: off\n"
        );
    }

    #[test]
    fn plugins_is_a_known_command() {
        assert_eq!(
            SlashCommand::from_name("plugins"),
            Some(SlashCommand::Plugins)
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Plugins));
        assert_eq!(SlashCommand::Plugins.name(), "plugins");
    }

    #[test]
    fn hidden_commands_are_completable_but_still_resolve() {
        let completable: Vec<&str> = SlashCommand::completable()
            .iter()
            .map(|c| c.name())
            .collect();
        for hidden in ["approve", "deny", "budget", "priority"] {
            assert!(
                !completable.contains(&hidden),
                "{hidden} must be hidden from completion"
            );
            assert!(
                SlashCommand::from_name(hidden).is_some(),
                "{hidden} must still resolve (anchor for re-entry)"
            );
        }
        // `all()` keeps them so `from_name` and the catalog assertions are intact.
        assert!(SlashCommand::all().iter().any(|c| c.name() == "approve"));
    }

    #[test]
    fn hidden_commands_report_a_reason_not_unknown() {
        assert_eq!(
            SlashCommand::Approve.unavailable_reason(),
            Some("子 agent 验收由父 agent 真实合并后 daemon 自动观察，无交互式权限审批")
        );
        assert_eq!(
            SlashCommand::Deny.unavailable_reason(),
            Some("子 agent 验收由父 agent 真实合并后 daemon 自动观察，无交互式权限审批")
        );
        assert_eq!(
            SlashCommand::Budget.unavailable_reason(),
            Some("daemon 尚未提供预算/优先级写接口")
        );
        assert_eq!(
            SlashCommand::Priority.unavailable_reason(),
            Some("daemon 尚未提供预算/优先级写接口")
        );
        // Exactly four commands are hidden: pin the boundary so a regression that
        // clears one of these reasons, or that marks a wired command unavailable,
        // fails here instead of slipping through an `is_some()`/`is_none()` check.
        assert_eq!(SlashCommand::Quit.unavailable_reason(), None);
        assert_eq!(SlashCommand::Cost.unavailable_reason(), None);
        assert_eq!(SlashCommand::Daemon.unavailable_reason(), None);
    }

    #[test]
    fn full_help_lists_side_commands_once_without_the_alias() {
        let help = help_text(None);
        assert!(help.contains("/side [问题]"), "目录必须列出 /side：{help}");
        assert!(help.contains("/back"), "目录必须列出 /back：{help}");
        assert!(
            help.contains("/side-close"),
            "目录必须列出 /side-close：{help}"
        );
        // btw 只作为别名出现在 /side 的描述里，不能自己占一行。
        assert!(!help.contains("  /btw"), "别名不能作为独立条目出现：{help}");
        assert_eq!(
            help.lines()
                .filter(|line| line.starts_with("  /side ["))
                .count(),
            1,
            "规范名只列一次：{help}"
        );
    }

    #[test]
    fn contextual_help_resolves_side_commands_but_not_the_alias_entry() {
        assert!(help_text(Some("side")).contains("/side [问题]"));
        assert!(help_text(Some("back")).contains("返回主会话"));
        assert!(help_text(Some("side-close")).contains("关闭当前旁问"));
        // `/btw` 是别名：查它应给出 /side 的说明，而不是「未知命令」。
        let btw = help_text(Some("btw"));
        assert!(btw.contains("/side [问题]"), "{btw}");
    }

    #[test]
    fn full_help_lists_only_completable_commands() {
        let help = help_text(None);
        for hidden in ["/approve", "/deny", "/budget", "/priority"] {
            assert!(!help.contains(hidden), "full help must not list {hidden}");
        }
        assert!(help.contains("/config") && help.contains("/model") && help.contains("/daemon"));
    }

    #[test]
    fn config_snapshot_renders_key_fields_without_secrets() {
        let snap = TuiConfigSnapshot {
            provider: "anthropic".into(),
            model_injected: false,
            workdir: std::path::PathBuf::from("/tmp/proj"),
            sandbox: "workspace-write".into(),
            yolo: false,
            max_turns: 200,
            compact_threshold: 160_000,
            mcp_master: true,
            runtime_preference: "ask".into(),
            runtime_preference_path: std::path::PathBuf::from(
                "/tmp/proj/.yi-agent/preferences.json",
            ),
            notifications: true,
            notification_method: "auto".into(),
            notification_condition: "unfocused".into(),
            effective_backend: "osc9".into(),
            terminal_title: true,
            title_status: "on".into(),
            focus_degraded: false,
            live: None,
        };
        let text = snap.render("claude-sonnet-4-5");
        assert!(text.contains("anthropic"));
        assert!(text.contains("claude-sonnet-4-5"));
        assert!(text.contains("/tmp/proj"));
        assert!(text.contains("workspace-write"));
        assert!(text.contains("160000") || text.contains("160_000"));
        // 通知行必须同时给出开关、解析后的实际后端与条件——只显示 `auto`
        // 时用户排查「为什么我的 kitty 不弹」没有任何线索。
        assert!(
            text.contains("terminal notify: on"),
            "config must show the notification switch: {text}"
        );
        assert!(
            text.contains("method=auto → osc9"),
            "config must show the resolved backend, not the raw setting: {text}"
        );
        assert!(
            text.contains("condition=unfocused"),
            "config must show the condition: {text}"
        );
        assert!(
            text.contains("terminal title: on"),
            "title status must be visible in /config: {text}"
        );
        assert!(!text.to_lowercase().contains("api_key"));
        assert!(!text.to_lowercase().contains("api-key"));
    }
    /// `/config` 必须显示保存后的**新**值，而不是启动快照。
    #[test]
    fn config_renders_live_values_after_a_save() {
        use yi_agent_runtime::live_settings::LiveSettings;
        use yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding;
        use yi_agent_runtime::live_settings::sources::FrozenRuntimeSources;

        let dir = tempfile::TempDir::new().unwrap();
        let binding = RuntimeSettingsBinding {
            live: LiveSettings::shared(dir.path().join("settings.json")),
            sources: std::sync::Arc::new(FrozenRuntimeSources::default()),
        };
        binding
            .live
            .commit(
                &serde_json::json!({ "agent.max_turns": 123 }),
                yi_agent_runtime::settings::store::PatchMode::ReplaceProvided,
            )
            .unwrap();

        let mut snap = snapshot_without_live();
        snap.live = Some(TuiLiveSettings::new(binding));
        let text = snap.render("claude-sonnet-4-5");

        assert!(text.contains("运行期设置"), "{text}");
        assert!(text.contains("max_turns: 123（settings）"), "{text}");
        assert!(text.contains("下一个安全执行点"), "{text}");
        // 未在文件里设置时显示代码默认值及其来源，而不是空。
        assert!(text.contains("compact ratio: 80（default）"), "{text}");
    }

    /// 显式启动来源遮蔽时，`/config` 要显示覆盖来源，而不是文件里的值。
    #[test]
    fn config_shows_a_host_override_instead_of_the_file_value() {
        use yi_agent_runtime::live_settings::LiveSettings;
        use yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding;
        use yi_agent_runtime::live_settings::sources::{FrozenRuntimeSources, ValueSource};

        let dir = tempfile::TempDir::new().unwrap();
        let binding = RuntimeSettingsBinding {
            live: LiveSettings::shared(dir.path().join("settings.json")),
            sources: std::sync::Arc::new(FrozenRuntimeSources::from_pairs(&[(
                "agent.max_turns",
                serde_json::json!(7),
                ValueSource::Override,
            )])),
        };
        binding
            .live
            .commit(
                &serde_json::json!({ "agent.max_turns": 123 }),
                yi_agent_runtime::settings::store::PatchMode::ReplaceProvided,
            )
            .unwrap();

        let mut snap = snapshot_without_live();
        snap.live = Some(TuiLiveSettings::new(binding));
        let text = snap.render("m");
        assert!(text.contains("max_turns: 7（override）"), "{text}");
        assert!(!text.contains("max_turns: 123"), "{text}");
    }

    /// 降级时说明当前用的是回退值并列出要修的键名，不回显任何取值。
    #[test]
    fn config_reports_degraded_health_without_echoing_values() {
        use yi_agent_runtime::live_settings::LiveSettings;
        use yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding;
        use yi_agent_runtime::live_settings::sources::FrozenRuntimeSources;

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"agent":{"max_turns":"TUI-SECRET"}}"#,
        )
        .unwrap();
        let binding = RuntimeSettingsBinding {
            live: LiveSettings::shared(dir.path().join("settings.json")),
            sources: std::sync::Arc::new(FrozenRuntimeSources::default()),
        };
        let mut snap = snapshot_without_live();
        snap.live = Some(TuiLiveSettings::new(binding));
        let text = snap.render("m");
        assert!(text.contains("配置读取异常"), "{text}");
        assert!(text.contains("agent.max_turns"), "{text}");
        assert!(!text.contains("TUI-SECRET"), "leaked: {text}");
    }

    /// 没有绑定的宿主不显示这一段：多印"当前值"会让用户以为保存立即生效。
    #[test]
    fn config_without_a_binding_omits_the_live_block() {
        let text = snapshot_without_live().render("m");
        assert!(!text.contains("运行期设置"), "{text}");
    }

    fn snapshot_without_live() -> TuiConfigSnapshot {
        TuiConfigSnapshot {
            provider: "anthropic".into(),
            model_injected: false,
            workdir: std::path::PathBuf::from("/tmp/proj"),
            sandbox: "workspace-write".into(),
            yolo: false,
            max_turns: 200,
            compact_threshold: 160_000,
            mcp_master: true,
            runtime_preference: "ask".into(),
            runtime_preference_path: std::path::PathBuf::from("/tmp/proj/.y"),
            notifications: true,
            notification_method: "auto".into(),
            notification_condition: "unfocused".into(),
            effective_backend: "bel".into(),
            terminal_title: true,
            title_status: "on".into(),
            focus_degraded: false,
            live: None,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: std::env::var("PROPTEST_CASES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256),
            ..ProptestConfig::default()
        })]

        /// 任意输入都不 panic，且返回值的规范名能再次解析回它自己。
        ///
        /// 这是 `from_name` 真正有意义的不变量。原稿写的
        /// 「`from_name(name).name() == name` 对所有 `all()` 元素成立」是恒真废话
        /// ——`all()` 里存的就是规范名，`find(|c| c.name() == name)` 直接命中，
        /// 永远为真、测不出任何东西。
        #[test]
        fn prop_from_name_converges_on_canonical_name(name in "\\PC{0,40}") {
            if let Some(cmd) = SlashCommand::from_name(&name) {
                let canonical = cmd.name();
                prop_assert_eq!(
                    SlashCommand::from_name(canonical).map(|c| c.name()),
                    Some(canonical),
                    "规范名必须能往返解析"
                );
            }
        }

        /// 规范名必须收敛：任何能被解析出来的名字，用它的 `name()` 再解析
        /// 必须回到同一个命令。这条保证补全、帮助与分发认的是同一份目录。
        #[test]
        fn prop_aliases_converge(name in "\\PC{0,60}") {
            let Some(first) = SlashCommand::from_name(&name) else { return Ok(()) };
            let second = SlashCommand::from_name(first.name());
            prop_assert_eq!(second, Some(first));
        }
    }
}
