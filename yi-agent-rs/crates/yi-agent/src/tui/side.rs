//! 原生 TUI 的旁支（`/side`、`/btw`）接入:定向命令/事件、双视图状态与按键裁决。
//!
//! 这个模块只做三件**与终端无关**的事,这样它们可以被确定性地测试:
//!
//! - `classify_conversation_input`:把一行输入精确分类成「主会话」还是
//!   「旁支动作」。命令判定必须发生在投递之前,否则运行中敲 `/side` 会先打断父;
//!   问题正文只裁掉命令前缀的命令边界,不改写用户原字节。
//! - `TuiSideView` / `TuiSideViews`:两份互不干扰的会话视图状态
//!   （草稿、滚动位置、运行态、错误），以及它们之间的切换。
//! - `route_approval_key`:父审批态下的显式命令编辑模式。它把「按 Enter
//!   批准」和「敲命令」彻底分开:命令模式下 Enter 只会执行命令。
//!
//! 真正的调度（`spawn_side_driver`）在 `crate::side_driver`,事件合流与渲染在
//! `tui::app`。这里不碰任何 I/O,所以下面的测试全部是纯函数调用。

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use yi_agent_runtime::side_conversation::{
    CloseReason, SideError, SideErrorReason, SideItem, SideStatus, SideUsage,
};

/// 当前可见的会话。
///
/// `Main` 只有一个,`Side` 至多一个（单父单旁支的设计约束）。带上
/// `generation` 是为了丢弃旧旁支的迟到帧:同一 `side_id` 被 close 后重新打开时,
/// 旧事件必须被识别成过期而不是写进新视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationTarget {
    Main,
    Side { side_id: String, generation: u64 },
}

impl ConversationTarget {
    pub fn is_side(&self) -> bool {
        matches!(self, Self::Side { .. })
    }

    /// 该目标是否匹配一个带标签的事件（`side_id` + `generation`）。
    ///
    /// 主会话事件不带标签:它们只可能来自主 driver,永远属于 `Main`。
    pub fn matches_side(&self, side_id: &str, generation: u64) -> bool {
        match self {
            Self::Main => false,
            Self::Side {
                side_id: current,
                generation: current_generation,
            } => current == side_id && *current_generation == generation,
        }
    }
}

/// 旁支事件在 TUI 侧的形状。
///
/// 正文与状态分成两个变体:正文是幂等的「当前可见项快照」（可整批 upsert）,
/// 状态是低频的枚举。二者分开之后,一个状态帧不会让正文重算,一个正文帧也
/// 不会把状态机往回拨。
#[derive(Debug, Clone)]
pub enum ConversationPayload {
    /// 主会话事件。
    ///
    /// **不带载荷**是刻意的:主事件本身直接写主视图的 `HistoryState`,这条
    /// 变体只用于在类型上标记「这是一条主事件」,让路由层有一个明确的分支,
    /// 而不是靠「我们记得不会有人把主事件发进旁支通道」的约定。
    Main,
    SideItems(Vec<Arc<SideItem>>),
    SideStatus(SideStatus),
    SideUsage(SideUsage),
    SideClosed(CloseReason),
}

/// 一条带目标标签的事件。
///
/// 标签不是装饰:消费者靠它分流,重连时客户端靠 `turn_id` 匹配 interrupt。
#[derive(Debug, Clone)]
pub struct ConversationEvent {
    pub target: ConversationTarget,
    pub turn_id: Option<String>,
    pub event: ConversationPayload,
}

/// 一行输入的去向。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedInput {
    /// 原文投给主会话。
    Main(String),
    /// 一个旁支动作,由 TUI 转成 `SideDriverCommand`。
    Side(SideInput),
}

/// 旁支动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideInput {
    /// 打开旁支,可自带第一个问题。
    Open { question: Option<String> },
    /// 返回主会话（不停止旁支）。
    Back,
    /// 关闭旁支（停止其活跃轮并释放资源）。
    Close,
    /// 在旁支里提一个问题。
    Submit(String),
    /// 旁支内可用的本地命令（`/cost`、`/config`、`/help`）。
    LocalCommand(String),
}

/// 旁支命令的规范名与别名。
///
/// `/btw` 不是独立命令,它只是 `/side` 的别名:帮助目录里只出现 `side` 一次,
/// 别名只用于精确识别输入。
pub const SIDE_COMMAND: &str = "side";
pub const SIDE_ALIAS: &str = "btw";
pub const BACK_COMMAND: &str = "back";
pub const SIDE_CLOSE_COMMAND: &str = "side-close";

/// `/help` 目录里的旁支条目。
///
/// `btw` **不**进目录:它是 `/side` 的别名,列两行同名功能只会让用户不知道该
/// 记哪个（桌面端 `SLASH_COMMANDS` 同一条规矩）。
pub const SIDE_HELP_ENTRIES: [(&str, &str); 3] = [
    (
        "side [问题]",
        "打开临时旁问（基于创建时上下文，只读）；/btw 是别名",
    ),
    ("back", "从旁问返回主会话，保留旁问及其在途轮次"),
    ("side-close", "关闭当前旁问（取消在途轮次）"),
];

/// 旁支视图中允许的本地命令。
///
/// `/cost` 在这里只统计旁支自身用量,不读父的费用累计。
pub const SIDE_LOCAL_COMMANDS: [&str; 3] = ["cost", "config", "help"];

fn invalid(message: impl Into<String>) -> SideError {
    SideError::new(SideErrorReason::InvalidParams, message)
}

/// 把一行输入精确分类。
///
/// 规则（与桌面/手机端共享同一套 token 语义）:
///
/// - 只认**首 token**。`"side bar"` 不是 `/side`,`"sidebar"` 更不是。
/// - `/btw` 是 `/side` 的别名,行为完全一致。
/// - `/back`、`/side-close` 不接受参数,带了参数是错误而不是「问题正文」:
///   静默忽略参数会让用户以为关掉了旁支。
/// - 旁支已经打开时,`/side` 是错误（不嵌套）。
/// - `/back`、`/side-close` **在任何视图都解析成功**:主会话里的语义由调用方
///   决定（`/back` 是「已经在主会话」的空操作提示,`/side-close` 关闭后台旁支,
///   没有旁支时幂等成功）。分类器只回答「这是不是旁支命令」,不替调用方猜
///   有没有可返回/可关闭的对象——那需要真实生命周期状态,而这里是纯函数。
pub fn classify_conversation_input(text: &str, in_side: bool) -> Result<RoutedInput, SideError> {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        // 旁支里敲的普通问题属于旁支。落到 `Main` 会把旁问送进父会话,
        // 那正是「后台/前台串线」最严重的一种。
        if in_side {
            return Ok(RoutedInput::Side(SideInput::Submit(text.to_string())));
        }
        return Ok(RoutedInput::Main(text.to_string()));
    };

    // 命令名到第一个空白为止;正文从空白之后开始。
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    let args = rest[name_end..].trim();

    match name {
        SIDE_COMMAND | SIDE_ALIAS => {
            if in_side {
                return Err(invalid("旁支不能嵌套:已经在一个旁支里了"));
            }
            Ok(RoutedInput::Side(SideInput::Open {
                question: (!args.is_empty()).then(|| args.to_string()),
            }))
        }
        BACK_COMMAND => {
            if !args.is_empty() {
                return Err(invalid("/back 不接受参数"));
            }
            Ok(RoutedInput::Side(SideInput::Back))
        }
        SIDE_CLOSE_COMMAND => {
            if !args.is_empty() {
                return Err(invalid("/side-close 不接受参数"));
            }
            Ok(RoutedInput::Side(SideInput::Close))
        }
        other if in_side && SIDE_LOCAL_COMMANDS.contains(&other) => {
            Ok(RoutedInput::Side(SideInput::LocalCommand(name.to_string())))
        }
        _ if in_side && !name.is_empty() => {
            // 旁支只读:管理类命令（clear/compact/model/...）在这里不被受理,
            // 但也不能把用户的话当问题发出去——保留原文,由调用方回填输入框。
            Ok(RoutedInput::Side(SideInput::Submit(text.to_string())))
        }
        _ => Ok(RoutedInput::Main(text.to_string())),
    }
}

/// 旁支事件帧要怎么落到视图上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideFrameEffect {
    /// 正文或状态变化,需要重绘。
    Redraw,
    /// 过期代际/非当前旁支的帧,直接丢弃。
    Discard,
}

/// 一个旁支视图的完整状态。
///
/// 刻意只装**状态**,不装渲染器:渲染仍走既有的 `HistoryState` /
/// `HistoryView`,两份视图共享同一套 renderer、缓存上限与 CJK 换行规则。
#[derive(Debug, Clone)]
pub struct TuiSideView {
    pub side_id: String,
    pub generation: u64,
    pub status: SideStatus,
    pub usage: SideUsage,
    /// 已应用的转录项（不重复 append,按 item id upsert）。
    pub items: Vec<Arc<SideItem>>,
    /// 失败/关闭原因。
    pub error: Option<String>,
    /// 关闭原因,`Some` 之后视图只读。
    pub closed: Option<CloseReason>,
    /// 是否有活跃轮。
    ///
    /// 草稿、滚动位置与待确认请求**不**在这里:它们分别由 `SideSession` 的
    /// `InputLine` / `HistoryState` / `pending_request` 持有。同一份状态存两处
    /// 必然分叉——按键改一处、渲染读另一处,用户看到的就是"草稿没了"。
    pub running: bool,
}

impl TuiSideView {
    pub fn new(side_id: String, generation: u64) -> Self {
        Self {
            side_id,
            generation,
            status: SideStatus::Creating,
            usage: SideUsage::default(),
            items: Vec::new(),
            error: None,
            closed: None,
            running: false,
        }
    }

    /// 应用一批转录项。
    ///
    /// 幂等:同一个 `id` 只更新不追加,`ordinal` 只增不减。这样流式 delta
    /// （每次都是同一 item 的完整当前值）不会把正文复制成多份。
    pub fn apply_items(&mut self, items: Vec<Arc<SideItem>>) {
        for item in items {
            match self
                .items
                .iter()
                .position(|existing| existing.id == item.id)
            {
                Some(index) => {
                    // 已完成的 item 不可变:迟到的旧值不能把它改回去。
                    if self.items[index].ordinal <= item.ordinal {
                        self.items[index] = item;
                    }
                }
                None => {
                    let position = self
                        .items
                        .iter()
                        .position(|existing| existing.ordinal > item.ordinal)
                        .unwrap_or(self.items.len());
                    self.items.insert(position, item);
                }
            }
        }
    }

    /// 应用一个带标签的帧。
    pub fn apply(
        &mut self,
        side_id: &str,
        generation: u64,
        payload: ConversationPayload,
    ) -> SideFrameEffect {
        if self.side_id != side_id || self.generation != generation {
            return SideFrameEffect::Discard;
        }
        match payload {
            ConversationPayload::SideItems(items) => {
                self.apply_items(items);
                SideFrameEffect::Redraw
            }
            ConversationPayload::SideStatus(status) => {
                if self.closed.is_some() {
                    return SideFrameEffect::Discard;
                }
                self.status = status;
                self.running = matches!(status, SideStatus::Running);
                SideFrameEffect::Redraw
            }
            ConversationPayload::SideUsage(usage) => {
                self.usage = usage;
                SideFrameEffect::Redraw
            }
            ConversationPayload::SideClosed(reason) => {
                self.closed = Some(reason);
                self.running = false;
                self.status = SideStatus::Closed;
                self.error = Some(close_reason_text(reason));
                SideFrameEffect::Redraw
            }
            // 主事件不会以 `Side` 目标送达;真发生了说明路由错了,丢弃。
            //
            // 这个变体必须存在：它让「主事件」在类型上只可能走主视图，路由层
            // 因此不必靠字符串/约定判断来源。
            ConversationPayload::Main => SideFrameEffect::Discard,
        }
    }

    /// 旁支是否还对用户可用。
    pub fn is_live(&self) -> bool {
        self.closed.is_none() && self.status != SideStatus::Closed
    }
}

/// 关闭原因的中文说明。与宿主无关,三端共用同一套措辞。
pub fn close_reason_text(reason: CloseReason) -> String {
    match reason {
        CloseReason::UserClosed => "旁支已关闭".to_string(),
        CloseReason::Expired => "旁支空闲超时，已自动关闭".to_string(),
        CloseReason::ParentCleared => "父会话已清空，旁支随之关闭".to_string(),
        CloseReason::ParentDeleted => "父会话已删除，旁支随之关闭".to_string(),
        CloseReason::HostShutdown => "宿主正在退出，旁支已关闭".to_string(),
        CloseReason::ResourceLost => "父会话的工作区已回收，旁支已关闭".to_string(),
    }
}

/// 主/旁两份视图。
///
/// `active` 决定哪一份接收按键与绘制,但**两份都保留**:从旁支 `back` 回来时
/// 父的草稿与滚动位置必须还在。
#[derive(Debug, Default)]
pub struct TuiSideViews {
    pub side: Option<TuiSideView>,
    pub active: Option<ConversationTarget>,
}

impl TuiSideViews {
    pub fn new() -> Self {
        Self {
            side: None,
            active: None,
        }
    }

    /// 当前是否在看旁支。
    pub fn in_side(&self) -> bool {
        matches!(self.active, Some(ConversationTarget::Side { .. }))
    }

    /// 打开/切换到一个旁支视图。
    ///
    /// 已存在同 `side_id`+`generation` 的视图时保留其状态（重复 open 共享同一
    /// 旁支,不能把已有正文清空）。
    pub fn open(&mut self, side_id: String, generation: u64) {
        let reuse = self
            .side
            .as_ref()
            .is_some_and(|view| view.side_id == side_id && view.generation == generation);
        if !reuse {
            self.side = Some(TuiSideView::new(side_id.clone(), generation));
        }
        self.active = Some(ConversationTarget::Side {
            side_id,
            generation,
        });
    }

    /// 回到主视图,保留旁支状态。
    pub fn back(&mut self) {
        self.active = Some(ConversationTarget::Main);
    }

    /// 把一份帧路由到正确的视图。
    ///
    /// 主事件只在**看主视图**时被交回调用方处理;旁支事件只写旁支视图。这条
    /// 规则是「后台父输出不污染旁支 history」的实现点。
    pub fn route(&mut self, event: &ConversationEvent) -> SideFrameEffect {
        match (&event.target, &event.event) {
            (
                ConversationTarget::Side {
                    side_id,
                    generation,
                },
                payload,
            ) => match self.side.as_mut() {
                Some(view) => view.apply(side_id, *generation, payload.clone()),
                None => SideFrameEffect::Discard,
            },
            (ConversationTarget::Main, ConversationPayload::Main) => SideFrameEffect::Redraw,
            _ => SideFrameEffect::Discard,
        }
    }
}

/// 可见的停止目标。
///
/// Ctrl+C 第一下只停**当前可见**会话:在旁支里就只停旁支,在主视图里就只停
/// 主会话。任一路都没在跑时返回 `None`,让调用方保持原有的退出确认语义。
pub fn visible_stop_target(
    active: &ConversationTarget,
    main_running: bool,
    side_running: bool,
) -> Option<ConversationTarget> {
    match active {
        ConversationTarget::Main => main_running.then_some(ConversationTarget::Main),
        target @ ConversationTarget::Side { .. } => side_running.then(|| target.clone()),
    }
}

/// TUI 退出时两路 runner 的收敛器。
///
/// 退出顺序是硬约束：先让旁支停下并 join，再让主会话收尾。旁支持有真实
/// owner（资源槽、blocking 工作、provider 流），如果它比主循环活得更久，
/// 那些资源会随进程一起被丢弃，而不是被释放。
///
/// 这里只做「通知 + 有界等待」，不知道任何 I/O 细节：旁支命令通道与
/// JoinHandle 由 `main` 注入，测试可以注入自己的假任务。
pub struct TuiExitCoordinator {
    side_commands: Option<tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>>,
    side_join: Option<tokio::task::JoinHandle<()>>,
}

impl TuiExitCoordinator {
    pub fn new(
        side_commands: Option<tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>>,
        side_join: Option<tokio::task::JoinHandle<()>>,
    ) -> Self {
        Self {
            side_commands,
            side_join,
        }
    }

    /// 没有旁支能力的宿主。
    pub fn disabled() -> Self {
        Self {
            side_commands: None,
            side_join: None,
        }
    }

    /// 通知旁支调度退出，并在有界时间内等它真的结束。
    ///
    /// 返回 `Ok(false)` 表示优雅退出超时（任务被强行留在后台）；调用方应把它
    /// 记成「未完成清理」，而不是宣称已经释放了资源。网络/阻塞 IO 本身不可
    /// 被杀，这一点不在本函数的承诺范围内。
    pub async fn stop_all(&mut self) -> Result<bool, SideError> {
        if let Some(commands) = &self.side_commands {
            // 旁支可能正忙，send 会等；退出不能因此无限挂住，给 2 秒。
            let sent = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                commands.send(crate::side_driver::SideDriverCommand::Exit),
            )
            .await
            .map_err(|_| SideError::new(SideErrorReason::SideClosing, "旁支退出通知超时"))?;
            if sent.is_err() {
                // 调度已经自行结束：这不是错误，join 会立刻返回。
            }
        }
        let Some(join) = self.side_join.take() else {
            return Ok(true);
        };
        match tokio::time::timeout(std::time::Duration::from_secs(5), join).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(_)) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}

/// 父审批态下的输入模式。
///
/// `Decision` 是历史行为（数字/Enter 直接裁决）;`Command` 是新增的显式模式,
/// 它把 `/` 之后的输入当命令而不是审批答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalInputMode {
    #[default]
    Decision,
    Command,
}

/// 一次按键在审批态下的裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalKeyAction {
    /// 交给既有的审批裁决逻辑。
    DecisionKey(KeyEvent),
    /// 命令模式下的编辑键（插入/删除/移动光标）。
    EditCommand(KeyEvent),
    /// 从裁决模式进入命令模式。
    EnterCommandMode,
    /// 退出命令模式,恢复审批裁决。
    ExitCommandMode,
    /// 应用级退出,任何模式都必须可达。
    ApplicationExit,
}

/// 命令模式下唯一能拆成「退出」的键:Ctrl+Q 与 Esc。
///
/// Esc 在命令模式里只退出命令模式（不退出应用）,Ctrl+Q 是应用级退出。
fn is_ctrl_q(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// 审批态按键路由。
///
/// 关键不变量:**命令模式下的 Enter 永远不是批准**。它被交回调用方执行当前的
/// 命令文本;只有 `Decision` 模式下 `Enter` 才是 `DecisionKey`。
pub fn route_approval_key(key: KeyEvent, mode: &mut ApprovalInputMode) -> ApprovalKeyAction {
    if is_ctrl_q(key) {
        return ApprovalKeyAction::ApplicationExit;
    }

    match (*mode, key.code) {
        (ApprovalInputMode::Decision, KeyCode::Char('/')) if key.modifiers.is_empty() => {
            *mode = ApprovalInputMode::Command;
            ApprovalKeyAction::EnterCommandMode
        }
        (ApprovalInputMode::Command, KeyCode::Esc) => {
            *mode = ApprovalInputMode::Decision;
            ApprovalKeyAction::ExitCommandMode
        }
        (ApprovalInputMode::Command, KeyCode::Enter) => ApprovalKeyAction::EditCommand(key),
        (ApprovalInputMode::Command, _) => ApprovalKeyAction::EditCommand(key),
        (ApprovalInputMode::Decision, _) => ApprovalKeyAction::DecisionKey(key),
    }
}

/// 顶部来源提示:旁支只读,并说明父会话此刻的状态。
///
/// 只陈述事实,不做裁剪:窄屏裁剪交给 renderer（它已经处理 CJK 宽度）。
pub fn side_header_line(parent_running: bool, parent_awaiting_approval: bool) -> String {
    let parent = if parent_awaiting_approval {
        "父会话等待审批"
    } else if parent_running {
        "父会话运行中"
    } else {
        "父会话空闲"
    };
    format!("旁问（只读）：继承父会话上下文 · {parent} · /back 返回 · /side-close 关闭")
}

#[cfg(test)]
mod tests;
