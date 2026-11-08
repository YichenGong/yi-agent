//! 主题与 git-diff 的后台通知泵。

use crate::protocol::Notification;
use std::sync::Arc;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{ViewFrames, write_notification};

/// 主题变化守望者:把 [`crate::theme_tool::ThemeHandle`] 的广播翻译成
/// `ui/settings/updated`,经 hub 扇出——stdio 的 `local` 与每个 ws 客户端都要收到。
///
/// 单独成形是为了可测:测试直接驱动这段生产循环,而不是照抄一份。
///
/// `Lagged` 必须当作**继续**:它只是订阅者一时落后(客户端读得慢),`recv()` 仍可
/// 继续调用;若像旧写法那样把它与 `Closed` 一并 `return`,一次背压就会杀掉守望者,
/// 此后本进程再也推不出主题通知。只有 `Closed`(所有发送端都没了)才结束。
pub(crate) async fn pump_theme_notifications(
    mut rx: tokio::sync::broadcast::Receiver<crate::settings_store::Theme>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    loop {
        let theme = match rx.recv().await {
            Ok(theme) => theme,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "theme watcher lagged; continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let n = Notification::UiSettingsUpdated {
            theme: theme.as_str().to_string(),
        };
        if write_notification(&hub, &n).await.is_err() {
            return;
        }
    }
}
/// Git Diff 焦点守望者：把 [`crate::git_diff_tool::GitDiffHandle`] 的广播翻译成
/// `ui/gitDiff/focus` 通知扇出。
///
/// 与 [`pump_theme_notifications`] 同构，`Lagged` 同样当作**继续**：它只是订阅者
/// 一时落后，`recv()` 仍可继续调用；只有 `Closed`（所有发送端都没了）才结束。
pub(crate) async fn pump_git_diff_notifications(
    mut rx: tokio::sync::broadcast::Receiver<crate::git_diff_tool::GitDiffFocus>,
    hub: Arc<crate::broadcast::Broadcaster>,
    // 会话帧表。watcher 与 `thread/view/read` 必须写读**同一张**表,否则面板
    // 读到的永远是空的;帧由工具算好随广播带来,这里只负责落表。
    view_frames: ViewFrames,
) {
    loop {
        let focus = match rx.recv().await {
            Ok(focus) => focus,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "git diff watcher lagged; continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        // 先落帧再发通知:通知到达前端时,前端随后的 `thread/view/read` 必须已经
        // 能读到这一帧,否则面板会先画一次旧帧(甚至空帧)。
        if let (Some(thread_id), Some(frame)) = (focus.thread_id.clone(), focus.frame) {
            view_frames.lock().await.insert(thread_id, *frame);
        }
        let n = Notification::UiGitDiffFocus {
            thread_id: focus.thread_id,
            note: focus.note,
        };
        if write_notification(&hub, &n).await.is_err() {
            return;
        }
    }
}

/// 观察到设置变化 → `settings/changed`（全局帧）。
///
/// 只消费**外部**变化（[`yi_agent_runtime::live_settings::LiveSettings::subscribe_external`]）：
/// 本进程自己经 `settings/write` 提交的那一次，由写请求自己广播（响应在前、
/// 通知紧跟），这里再播一遍就是同版本重复。分开两条通道而不是事后比较闩锁，
/// 是因为「提交完成、通知尚未发出」之间必然有一个窗口，观察线程若在那时读到
/// 新版本，任何比较式闩锁都会被它抢先。
///
/// 通知**不带任何取值**，只带 `revision` 与逻辑变化的键名。健康状态翻转也走这条
/// 帧：revision 可以不变、`changed_keys` 为空，客户端据此重新 read 并看 `health`。
pub(crate) async fn pump_settings_notifications(
    live: Arc<yi_agent_runtime::live_settings::LiveSettings>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    // 顺序是「取基线 → 订阅 → 补齐」，三步都不能省：
    //
    // * 先取基线：后面比较的对象必须是「订阅之前」的内容，否则订阅之后立刻
    //   收到的那条唤醒没有可比对的旧值。
    // * 再订阅：`borrow_and_update` 把当前序号标为已见，避免启动瞬间凭空播一条。
    // * 最后补齐：`refresh()` 是**先发布、后通知**（观察线程、别的进程的保存都
    //   走这条路径），因此存在「已发布、但通知早于我们的订阅」的窗口。只靠
    //   `changed()` 会永久丢掉那一帧，补齐这一步就是把它捞回来。
    //
    // 反过来的顺序（先订阅、后取基线）同样有窗口：基线读到的已经是新内容，
    // 随后 `changed()` 触发时与新内容相等被判重复，那一帧照样丢掉。
    let mut last = live.published().unwrap_or_else(|| live.refresh());
    let mut changes = live.subscribe_external();
    changes.borrow_and_update();
    // 补齐：订阅之前就已经发布的内容，若与基线不同，当场播一次。
    if let Some(next) = live.published()
        && differs(&last, &next)
    {
        let notification = Notification::SettingsChanged {
            revision: next.revision.clone(),
            changed_keys: yi_agent_runtime::settings::store::changed_keys_between(
                &last.settings,
                &next.settings,
            ),
        };
        last = next;
        if write_notification(&hub, &notification).await.is_err() {
            return;
        }
    }
    loop {
        if changes.changed().await.is_err() {
            return;
        }
        // 用**已发布**快照，不再自己读盘：观察线程发布的正是触发这次唤醒的那份，
        // 再 refresh 一次既浪费一次 I/O，也会把「本次唤醒对应哪份内容」变成两次
        // 独立读取之间的猜测。
        let Some(next) = live.published() else {
            continue;
        };
        // 没有可见差异（内容、来源、健康都相同）就不发：观察线程可能因为
        // 别的原因唤醒过，重复帧只会让所有客户端白读一遍。
        if !differs(&last, &next) {
            last = next;
            continue;
        }
        let notification = Notification::SettingsChanged {
            revision: next.revision.clone(),
            changed_keys: yi_agent_runtime::settings::store::changed_keys_between(
                &last.settings,
                &next.settings,
            ),
        };
        last = next;
        // 广播失败不结束泵：设置仍然有效，下一个变化还要继续送达。
        if write_notification(&hub, &notification).await.is_err() {
            return;
        }
    }
}

/// 两份快照之间有没有客户端可见的差别。
///
/// 三项都要比：`revision` 抓内容变化，`origin` 抓「内容相同但来源从当前变成
/// 最后有效值」，`health` 抓「内容没变但文件已经解析不了」。只比 revision 会
/// 让健康降级静默消失——那正是客户端最需要被通知的一类变化。
fn differs(
    previous: &yi_agent_runtime::live_settings::SettingsSnapshot,
    next: &yi_agent_runtime::live_settings::SettingsSnapshot,
) -> bool {
    previous.revision != next.revision
        || previous.origin != next.origin
        || previous.health != next.health
}
