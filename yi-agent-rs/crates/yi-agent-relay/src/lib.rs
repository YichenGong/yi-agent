//! 反向 WSS 中继:两端都出站连接,中继按 `session` 配对并双向转发帧。
//!
//! 中继**不解析 JSON-RPC 语义**:它只保证「同一 session 的 App 帧送到该 session
//! 的电脑连接,反之亦然」(spec §11.1)。这让中继极薄、可无状态重启(spec §11.4)。
//!
//! 路由规则:
//! - `agent → app`:扇出给该 session 的**全部** App 连接;
//! - `app → agent`:送给该 session 的**唯一**电脑连接;不存在则回一个错误帧;
//! - 断连即摘除注册(spec §6:一个端点的故障绝不拖垮中继或别的 session)。
//!
//! 一个 `session` 视为一「配对会话」:电脑侧建立 session 并 [`Relay::attach_agent`],
//! 手机扫码后以同一 session [`Relay::attach_app`]。中继只做搬运,不做认证/加密
//! (v1 靠两端 WSS 的 TLS;端到端加密列 v1.1)。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

pub mod backoff;
pub mod client;
pub mod server;

pub use client::run_client;

/// 一个中继端点(电脑或手机)的出站队列容量。写满即视为慢消费者。
pub const ENDPOINT_QUEUE: usize = 256;

/// 一端送帧**入**中继的入站队列容量。
pub const INBOUND_QUEUE: usize = 256;

/// 中继搬运的帧。`axum::extract::ws::Message` 是 axum 服务端与 tungstenite 客户端
/// 都认得的最小公共表示,故直接采用它,避免再包一层自定义枚举。
pub type Message = axum::extract::ws::Message;

/// 一个无法投递的 `app → agent` 帧回给发起 App 的错误码(JSON-RPC server error 段)。
const NO_AGENT_ERROR_CODE: i64 = -32000;

/// 一个已登记端点的出站口 + 它的唯一登记号。
///
/// 登记号用于「摘除时只摘自己」:电脑/手机可能重连,新连接会顶替旧连接在表里的
/// 位置;旧连接的任务随后收尾时,若按 session 直接把条目删掉,会误删新连接。
/// 每个连接一个单调递增的 token 即可精确判断「我现在还是不是表里那一个」。
struct Slot {
    sink: mpsc::Sender<Message>,
    token: u64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 一个按 `session` 配对电脑端与手机端的极薄转发器。
///
/// 无状态设计:注册表全在内存,进程重启即从空表重建(spec §11.4)。
#[derive(Default)]
pub struct Relay {
    /// `session_id → 该 session 的唯一电脑连接`。
    agents: Mutex<HashMap<String, Slot>>,
    /// `session_id → 该 session 的全部手机连接`(可多条)。
    apps: Mutex<HashMap<String, Vec<Slot>>>,
    /// 端点登记号计数器。
    next_token: AtomicU64,
}

impl Relay {
    pub fn new() -> Self {
        Self::default()
    }

    fn take_token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::Relaxed)
    }

    /// 登记一台电脑连接,并把该 session 里 App 送来的帧转发给它。
    ///
    /// 参数语义:
    /// - `sink`:中继 → 电脑 的**出站**口(中继把 `app → agent` 的帧写这里);
    /// - `from_agent`:电脑 → 中继 的**入站**口(中继从这里取帧并扇出给 App)。
    ///
    /// 同一 session 若已有电脑连接,新的**顶替**旧的(电脑重连,中继无状态)。
    /// 本函数在连接存续期间一直运行(`from_agent` 关闭即返回),返回时按需摘除注册。
    pub async fn attach_agent(
        &self,
        session: String,
        sink: mpsc::Sender<Message>,
        mut from_agent: mpsc::Receiver<Message>,
    ) {
        let token = self.take_token();
        lock(&self.agents).insert(session.clone(), Slot { sink, token });

        // 电脑 → 手机:扇出到该 session 的全部 App。
        while let Some(frame) = from_agent.recv().await {
            self.fan_out_to_apps(&session, frame);
        }

        // 断连:只摘除「仍是我」的登记,不误删可能已顶替我的新连接。
        let mut agents = lock(&self.agents);
        if agents.get(&session).map(|s| s.token) == Some(token) {
            agents.remove(&session);
        }
    }

    /// 登记一台手机连接,并把该 session 里电脑送来的帧转发给它。
    ///
    /// 参数语义:
    /// - `sink`:中继 → 手机 的**出站**口(中继把 `agent → app` 的帧写这里);
    /// - `from_app`:手机 → 中继 的**入站**口(中继从这里取帧并送给电脑)。
    ///
    /// 手机可多条共存:它们都收到同一份电脑帧(`agent → app` 扇出),任一条发起的
    /// `app → agent` 都会送到该 session 唯一的电脑连接。
    pub async fn attach_app(
        &self,
        session: String,
        sink: mpsc::Sender<Message>,
        mut from_app: mpsc::Receiver<Message>,
    ) {
        let token = self.take_token();
        lock(&self.apps)
            .entry(session.clone())
            .or_default()
            .push(Slot { sink, token });

        // 手机 → 电脑:送给该 session 唯一的电脑连接;不存在则回一个错误帧。
        while let Some(frame) = from_app.recv().await {
            let agent = lock(&self.agents).get(&session).map(|s| s.sink.clone());
            match agent {
                Some(tx) => {
                    // 电脑连接若恰在此刻消失,本帧丢弃;下一帧会走 no-agent 错误分支。
                    let _ = tx.send(frame).await;
                }
                None => self.reply_no_agent(&session, frame),
            }
        }

        // 断连:摘除「仍是我」的那条 App 登记;该 session 空了就清掉整个条目。
        let mut apps = lock(&self.apps);
        if let Some(list) = apps.get_mut(&session) {
            list.retain(|s| s.token != token);
            if list.is_empty() {
                apps.remove(&session);
            }
        }
    }

    /// 把一帧扇出给该 session 的全部 App;已关闭或**写满(慢消费者)**的连接就地剔除。
    ///
    /// 队列写满即摘除该连接,与 app-server 的 `Broadcaster::broadcast` 同源:对一条
    /// 已经跟不上的连接,重连 + `thread/resume` 增量回放(spec §5.2/§11.4)是设计好
    /// 的恢复路径,好过在 JSON-RPC 流中间静默丢一帧(那会让对端永远等一个响应)。
    fn fan_out_to_apps(&self, session: &str, frame: Message) {
        let mut apps = lock(&self.apps);
        if let Some(list) = apps.get_mut(session) {
            list.retain(|s| s.sink.try_send(frame.clone()).is_ok());
            if list.is_empty() {
                apps.remove(session);
            }
        }
    }

    /// 没有电脑可用时,回一个最小错误帧给发起 App(不解析其 JSON-RPC 语义)。
    fn reply_no_agent(&self, session: &str, _frame: Message) {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "error": {
                "code": NO_AGENT_ERROR_CODE,
                "message": "no computer connected for this session",
            },
        });
        let frame = Message::Text(payload.to_string().into());
        let mut apps = lock(&self.apps);
        if let Some(list) = apps.get_mut(session) {
            list.retain(|s| s.sink.try_send(frame.clone()).is_ok());
        }
    }

    /// 观测:某 session 当前是否有电脑连接。
    pub fn has_agent(&self, session: &str) -> bool {
        lock(&self.agents).contains_key(session)
    }

    /// 观测:某 session 当前已登记的手机连接数。
    pub fn app_count(&self, session: &str) -> usize {
        lock(&self.apps).get(session).map(Vec::len).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// 起一个 `Arc<Relay>` 并挂上一条电脑连接,返回(relay, 送给电脑的 recv, 电脑
    /// 向中继送帧的 tx)。各任务在返回的 `Arc` 存活期间常驻。
    fn attach_agent(
        relay: &Arc<Relay>,
        session: &str,
    ) -> (
        mpsc::Receiver<Message>,
        mpsc::Sender<Message>,
        tokio::task::JoinHandle<()>,
    ) {
        let (agent_out_tx, agent_out_rx) = mpsc::channel(ENDPOINT_QUEUE);
        let (agent_in_tx, agent_in_rx) = mpsc::channel(INBOUND_QUEUE);
        let r = Arc::clone(relay);
        let s = session.to_string();
        let handle =
            tokio::spawn(async move { r.attach_agent(s, agent_out_tx, agent_in_rx).await });
        (agent_out_rx, agent_in_tx, handle)
    }

    /// 起一条手机连接,返回(送给手机的 recv, 手机向中继送帧的 tx, 任务句柄)。
    fn attach_app(
        relay: &Arc<Relay>,
        session: &str,
    ) -> (
        mpsc::Receiver<Message>,
        mpsc::Sender<Message>,
        tokio::task::JoinHandle<()>,
    ) {
        let (app_out_tx, app_out_rx) = mpsc::channel(ENDPOINT_QUEUE);
        let (app_in_tx, app_in_rx) = mpsc::channel(INBOUND_QUEUE);
        let r = Arc::clone(relay);
        let s = session.to_string();
        let handle = tokio::spawn(async move { r.attach_app(s, app_out_tx, app_in_rx).await });
        (app_out_rx, app_in_tx, handle)
    }

    async fn recv(rx: &mut mpsc::Receiver<Message>) -> Message {
        tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timed out waiting for a relayed frame")
            .expect("relay channel closed unexpectedly")
    }

    /// 断言在短窗口内**收不到**帧。
    async fn assert_quiet(rx: &mut mpsc::Receiver<Message>) {
        assert!(
            tokio::time::timeout(Duration::from_millis(80), rx.recv())
                .await
                .is_err(),
            "expected no frame, but one arrived"
        );
    }

    fn text(m: &Message) -> String {
        match m {
            Message::Text(t) => t.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    /// 让登记生效(attach 是异步任务,insert 在其首行)。
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    /// 同 session:App 发的帧到达该 session 的电脑。
    #[tokio::test]
    async fn an_app_frame_reaches_its_session_agent() {
        let relay = Arc::new(Relay::new());
        let (mut agent_out, agent_in, _agent_task) = attach_agent(&relay, "s1");
        let (_app_out, app_in, _app_task) = attach_app(&relay, "s1");
        settle().await;
        assert!(relay.has_agent("s1"));
        assert_eq!(relay.app_count("s1"), 1);

        app_in.send(Message::Text("from-app".into())).await.unwrap();
        assert_eq!(text(&recv(&mut agent_out).await), "from-app");
        let _ = agent_in;
    }

    /// 同 session:电脑发的帧扇出到**全部** App。
    #[tokio::test]
    async fn an_agent_frame_fans_out_to_all_apps() {
        let relay = Arc::new(Relay::new());
        let (_agent_out, agent_in, _agent_task) = attach_agent(&relay, "s1");
        let (mut app_a_out, _app_a_in, _a) = attach_app(&relay, "s1");
        let (mut app_b_out, _app_b_in, _b) = attach_app(&relay, "s1");
        settle().await;
        assert_eq!(relay.app_count("s1"), 2);

        agent_in
            .send(Message::Text("from-agent".into()))
            .await
            .unwrap();
        assert_eq!(text(&recv(&mut app_a_out).await), "from-agent");
        assert_eq!(text(&recv(&mut app_b_out).await), "from-agent");
    }

    /// 两个 session 完全隔离:一个 session 的帧不得漏到另一个 session。
    #[tokio::test]
    async fn two_sessions_are_isolated() {
        let relay = Arc::new(Relay::new());
        let (mut a_out, a_in, _a_task) = attach_agent(&relay, "s1");
        let (mut b_out, _b_in, _b_task) = attach_agent(&relay, "s2");
        let (mut app1_out, app1_in, _app1) = attach_app(&relay, "s1");
        let (mut app2_out, _app2_in, _app2) = attach_app(&relay, "s2");
        settle().await;

        // s1 的 App → 只应到 s1 的电脑。
        app1_in.send(Message::Text("to-s1".into())).await.unwrap();
        assert_eq!(text(&recv(&mut a_out).await), "to-s1");
        assert_quiet(&mut b_out).await;

        // s1 的电脑 → 只应到 s1 的 App,不到 s2 的 App。
        a_in.send(Message::Text("from-s1".into())).await.unwrap();
        assert_eq!(text(&recv(&mut app1_out).await), "from-s1");
        assert_quiet(&mut app2_out).await;
    }

    /// `app → agent` 时该 session 没有电脑:App 收到一个错误帧,而非静默丢失。
    #[tokio::test]
    async fn an_app_frame_without_a_computer_gets_an_error_frame() {
        let relay = Arc::new(Relay::new());
        let (mut app_out, app_in, _app) = attach_app(&relay, "lonely");
        settle().await;
        assert!(!relay.has_agent("lonely"));

        app_in.send(Message::Text("hello?".into())).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&text(&recv(&mut app_out).await)).unwrap();
        assert_eq!(v["error"]["code"], NO_AGENT_ERROR_CODE);
    }

    /// 断连摘除:电脑连接关闭后,中继不再认为该 session 有电脑。
    #[tokio::test]
    async fn a_disconnected_agent_is_deregistered() {
        let relay = Arc::new(Relay::new());
        let (_agent_out, agent_in, agent_task) = attach_agent(&relay, "s1");
        settle().await;
        assert!(relay.has_agent("s1"));

        // 电脑断开:入站 tx 丢弃、入站 rx 关闭 → attach_agent 收尾摘除。
        drop(agent_in);
        agent_task.await.unwrap();
        assert!(!relay.has_agent("s1"), "a gone agent must be deregistered");
    }

    /// 断连摘除:手机连接关闭后,该 session 的 App 计数归零、条目清空。
    #[tokio::test]
    async fn a_disconnected_app_is_deregistered() {
        let relay = Arc::new(Relay::new());
        let (_app_out, app_in, app_task) = attach_app(&relay, "s1");
        settle().await;
        assert_eq!(relay.app_count("s1"), 1);

        drop(app_in);
        app_task.await.unwrap();
        assert_eq!(relay.app_count("s1"), 0);
    }

    /// 电脑重连:新连接顶替旧连接,帧送到**新**连接。
    #[tokio::test]
    async fn a_reconnected_agent_replaces_the_old_one() {
        let relay = Arc::new(Relay::new());
        let (_old_out, old_in, _old_task) = attach_agent(&relay, "s1");
        settle().await;

        // 新电脑连接顶替。
        let (mut new_out, _new_in, _new_task) = attach_agent(&relay, "s1");
        settle().await;

        let (_app_out, app_in, _app) = attach_app(&relay, "s1");
        settle().await;
        app_in
            .send(Message::Text("after-reconnect".into()))
            .await
            .unwrap();
        assert_eq!(text(&recv(&mut new_out).await), "after-reconnect");

        // 旧连接此刻收尾,不得把新连接从表里抹掉。
        drop(old_in);
        settle().await;
        assert!(
            relay.has_agent("s1"),
            "the old agent's teardown must not evict the new one"
        );
    }

    /// 慢消费者(出站队列写满)的连接在扇出时被摘除,与会阻塞的同伴互不影响。
    ///
    /// 与 app-server `Broadcaster` 的背压语义一致:跟不上的连接被摘除,重连 +
    /// `thread/resume` 是恢复路径。
    #[tokio::test]
    async fn a_slow_app_is_dropped_from_the_fan_out() {
        let relay = Arc::new(Relay::new());
        let (_agent_out, agent_in, _agent_task) = attach_agent(&relay, "s1");
        // 慢 App:注册后从不 recv,队列很快写满。
        let (_slow_out, _slow_in, _slow_task) = attach_app(&relay, "s1");
        // 快 App:持续排空。
        let (mut fast_out, _fast_in, _fast_task) = attach_app(&relay, "s1");
        settle().await;
        assert_eq!(relay.app_count("s1"), 2);

        // 灌满慢消费者的队列并越过容量;快消费者边收边排空。
        for i in 0..(ENDPOINT_QUEUE + 8) {
            agent_in
                .send(Message::Text(format!("f{i}").into()))
                .await
                .unwrap();
            // 给扇出任务一点时间推进,并排空快消费者。
            tokio::time::sleep(Duration::from_millis(1)).await;
            let _ = fast_out.try_recv();
        }
        // 慢消费者被摘除,只剩快消费者。
        assert_eq!(
            relay.app_count("s1"),
            1,
            "a slow app must be dropped from the session"
        );
    }
}
