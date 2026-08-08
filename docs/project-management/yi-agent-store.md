# yi-agent-store

## 模块说明

yi-agent 的会话持久化 crate。目标是保存和恢复 `yi-agent-core::Session`，使会话可跨进程继续。

## 范围边界

**做什么：**
- 本地会话历史的保存与读取
- 基于已恢复 `Session` 重建 agent
- 存储后端与数据格式的封装

**不做什么：**
- 不在本 crate 执行 Agent 循环
- 不将会话上传至远程服务

## Features

- [ ] Session 持久化与恢复 — 当前仅有 `crates/yi-agent-store/src/lib.rs` crate 骨架，尚无存储后端或 save/load API
