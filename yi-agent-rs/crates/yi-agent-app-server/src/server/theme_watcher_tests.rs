//! 主题守望者的回归测试。
//!
//! 这些测试直接驱动生产里的 [`pump_theme_notifications`],不再照抄一份循环体:
//! 「Lagged 后仍存活」只有在真正跑被 spawn 的那段代码时才算数。

use super::*;

/// 快进:让订阅者跑赢容量 16 的广播、把订阅者落在后面,`recv()` 就会返回
/// `Lagged`(`ThemeHandle::new` 用的正是 `broadcast::channel(16)`)。
///
/// 先 `yield_now` 保证 spawn 的守望者已挂上订阅:否则 17 次 `send` 一场空,
/// 之后再订阅只会收到第 201 次的值,测不出 Lagged。
async fn lag_the_subscriber(theme: &crate::theme_tool::ThemeHandle) {
    tokio::task::yield_now().await;
    for i in 0..(16 + 200) {
        theme.set(if i % 2 == 0 {
            crate::settings_store::Theme::Light
        } else {
            crate::settings_store::Theme::Dark
        });
    }
}

/// 回归:`Lagged` 不得终结守望者。
///
/// 旧写法 `while let Ok(theme) = rx.recv().await` 把 `Lagged` 与 `Closed` 一并
/// 当作退出条件;一次背压(客户端一时读得慢)就把守望者杀掉,此后本进程再也
/// 推不出 `ui/settings/updated`——对话框或 `set_theme` 工具改的主题静默丢失。
#[tokio::test]
async fn theme_watcher_survives_a_lagged_broadcast() {
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let local = crate::broadcast::ClientId::local();
    let mut out = hub.register_reliable(local);
    let theme = crate::theme_tool::ThemeHandle::new(std::env::temp_dir());
    let pump = tokio::spawn(pump_theme_notifications(
        theme.subscribe(),
        Arc::clone(&hub),
    ));

    lag_the_subscriber(&theme).await;

    // 让守望者重新比发送者快:它必须先处理 `Lagged`(不退出),再收到这次值。
    let mut saw_light_after_the_lag = false;
    for _ in 0..200 {
        theme.set(crate::settings_store::Theme::Light);
        match tokio::time::timeout(crate::test_support::EVENT_WAIT, out.recv()).await {
            Ok(Some(frame)) => {
                if frame["method"] == "ui/settings/updated" && frame["params"]["theme"] == "light" {
                    saw_light_after_the_lag = true;
                    break;
                }
            }
            Ok(None) => panic!("the local client's outbound queue closed unexpectedly"),
            Err(_) => break,
        }
    }
    pump.abort();
    assert!(
        saw_light_after_the_lag,
        "the theme watcher must stay alive across a Lagged broadcast and still push \
         ui/settings/updated; `while let Ok` returns instead"
    );
}

/// 守住的另一半语义:`Closed`(所有发送端都没了)才终止。
///
/// 这条防止把守望者改成「永不退出」的忙循环。
#[tokio::test]
async fn theme_watcher_ends_when_the_broadcast_closes() {
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let (tx, rx) = tokio::sync::broadcast::channel::<crate::settings_store::Theme>(16);
    let pump = tokio::spawn(pump_theme_notifications(rx, Arc::clone(&hub)));
    drop(tx);
    let finished = tokio::time::timeout(crate::test_support::EVENT_WAIT, pump)
        .await
        .expect("the watcher must exit once the broadcast is closed");
    assert!(finished.is_ok());
}

/// 投递分层：列表层/全局帧无 thread 键（恒推），内容层按 thread 键过滤。
#[test]
fn notification_delivery_classifies_list_and_content() {
    use crate::protocol::{Delivery, Notification};
    let status = Notification::ThreadStatusUpdated {
        thread_id: "t2".into(),
        status: crate::protocol::ThreadStatus::Running,
    };
    assert_eq!(status.delivery(), Delivery::List);
    assert_eq!(
        Notification::ThreadStarted {
            thread_id: "t2".into(),
            cwd: "/w".into(),
            model: "m".into(),
            model_ref: None
        }
        .delivery(),
        Delivery::List
    );
    assert_eq!(
        Notification::ItemDelta {
            thread_id: "t2".into(),
            item_id: "i".into(),
            delta: "d".into()
        }
        .delivery(),
        Delivery::Content
    );
    // 回合结束是一桩**列表层/注意力**事实，不是会话正文：未读蓝点唯一的
    // 驱动。远程客户端只订阅寥寥几条会话，若把它按内容层过滤，窗口外跑完的
    // 会话就永远不亮蓝点——用户以为「没跑完」。
    assert_eq!(
        Notification::TurnCompleted {
            thread_id: "t2".into(),
            turn_id: "u".into(),
            status: crate::protocol::TurnStatus::Completed,
            error: None,
            completed_at: None
        }
        .delivery(),
        Delivery::List
    );
    assert_eq!(
        Notification::UiSettingsUpdated {
            theme: "dark".into()
        }
        .delivery(),
        Delivery::Global
    );
    assert_eq!(
        Notification::Error {
            message: "x".into()
        }
        .delivery(),
        Delivery::Global
    );
    assert_eq!(
        Notification::ToolCallApprovalResolved {
            perm_id: "p".into(),
            by: "c".into(),
            decision: "allow_once".into()
        }
        .delivery(),
        Delivery::Global
    );
}

/// 订阅 t1 的客户端：仍收到 t2 的 `thread/status/updated`（列表层恒推），
/// 但收不到 t2 的 `item/delta`（内容层过滤）。
#[tokio::test(flavor = "multi_thread")]
async fn list_layer_is_always_delivered_while_content_is_filtered() {
    let hub = crate::broadcast::Broadcaster::new();
    let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(1));
    let mut rx = hub.register(sub.clone());
    hub.subscribe(&sub, vec!["t1".to_string()]);

    // 列表层：未订阅的 t2 的状态帧也必须到达。
    write_notification(
        &hub,
        &Notification::ThreadStatusUpdated {
            thread_id: "t2".into(),
            status: crate::protocol::ThreadStatus::Running,
        },
    )
    .await
    .unwrap();
    assert_eq!(rx.recv().await.unwrap()["params"]["thread_id"], "t2");

    // 列表层：未订阅的 t2 的**回合结束**同样必须到达——它是未读蓝点唯一的
    // 驱动，被过滤掉就再没有第二次机会（内容层被挡下，客户端无从得知跑完了）。
    // 用 `try_recv` 而非 `recv().await`：投递是同步 `try_send`，帧已在该分支
    // 入队或丢弃；`await` 在 RED 下会挂死而不是快速失败。
    write_notification(
        &hub,
        &Notification::TurnCompleted {
            thread_id: "t2".into(),
            turn_id: "u1".into(),
            status: crate::protocol::TurnStatus::Completed,
            error: None,
            completed_at: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rx.try_recv().expect("未订阅的 t2 的回合结束帧也必须到达")["method"],
        "turn/completed"
    );

    // 内容层：未订阅的 t2 的 delta 必须被挡下。
    write_notification(
        &hub,
        &Notification::ItemDelta {
            thread_id: "t2".into(),
            item_id: "i".into(),
            delta: "leak".into(),
        },
    )
    .await
    .unwrap();
    assert!(rx.try_recv().is_err(), "订阅 t1 不该收到 t2 的内容帧");
}
