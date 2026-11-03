# yi-agent-relay

## 模块说明

按 session 将唯一电脑连接与多台手机连接配对的反向 WebSocket 中继；不解析 JSON-RPC 语义。部署入口见 [中继部署](../../deploy/relay/README.md)。

## 本次修复

- [x] 长历史回放遇到短时满队列不丢帧 — `yi-agent-rs/crates/yi-agent-relay/src/lib.rs::Relay::fan_out_to_apps` 对各手机并发 `send`，单次等待上限 10 秒；放开注册表锁后等待，当前帧投递落定再接下一帧，保持顺序与有界队列。原先 `try_send` 在 256 帧队列写满时直接摘除手机，丢掉后续历史与 resume 回包。判据：`cd yi-agent-rs && cargo test -p yi-agent-relay a_replay_burst_survives_a_temporarily_full_app_queue`（600 帧历史与末尾响应完整、按序送达），`cargo test -p yi-agent-relay a_full_phone_queue_does_not_block_fast_delivery_or_other_sessions`（快手机及时收到本帧、其他 session 独立继续传输）。同 session 后续帧最多等待慢端 10 秒，超时后移除慢端；不是无限等待。
- [x] 持续慢消费者明确断开，不保留假连接 — `lib.rs::Relay::attach_app` 接收专属退出信号；投递失败按登记 token 摘除并终止入站任务。`src/server.rs::bridge_app` 在读泵、写泵或路由任务任一退出时结束整条 socket，并让路由正常收尾；原先 `join!(read, write)` 会等待仍存活的读半，手机仍以为已连接。判据：`cargo test -p yi-agent-relay a_slow_app_is_disconnected_after_the_delivery_deadline`（任务有界结束、入站关闭）、`cargo test -p yi-agent-relay a_closed_phone_exits_even_while_its_request_waits_for_the_agent`（电脑队列满时手机断线仍可收尾）；`cargo test -p yi-agent-relay`、`cargo clippy -p yi-agent-relay --all-targets -- -D warnings`。

## 部署与验收

源码修复不代表已部署。必须重新构建并更新 VPS 的 `yi-agent-relay`；前端回放看门狗的生命周期修复还需要更新手机前端包（见 [desktop](./desktop.md)）。不改变域名/session/配对凭据，也不需要重新配对。更新中继会短暂断开现有连接，应选择合适窗口。

真机待验：弱网打开长历史，正文完整加载且连接状态收敛；中继日志若出现 `app delivery stalled or closed; disconnecting for resync`，手机应断开并自动重连，而不是一直显示已连接。
