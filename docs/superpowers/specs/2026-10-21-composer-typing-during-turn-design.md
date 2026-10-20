# 回合进行中允许在输入框打字

日期: 2026-10-21
状态: 已批准

## 问题

一个 turn 正在跑时,输入框被 `disabled`,用户完全无法打字。想追加或改写指令,
只能先点 Stop 打断整轮,再重新输入——代价是丢掉已经产生的半轮工作。

## 目标

turn 进行中,输入框**可编辑**;但**发送仍然必须先按 Stop**。

## 非目标

- 不改服务端 `turn/interject` 语义,也不在 UI 上暴露 interject 通道。
- 不改 Stop 按钮的位置、颜色、可用条件。
- 不新增排队发送(auto-send after turn ends)。

## 行为规格

| 场景 | turn 进行中 | turn 空闲 |
| --- | --- | --- |
| 输入框可编辑 | 是 | 是 |
| 草稿保留 | 是,整轮结束/中断后仍在 | 是 |
| 按 Enter(桌面) | **中断当前回合**(现状不变) | 发送 |
| Shift+Enter | 换行 | 换行 |
| 斜杠建议弹窗 | 不弹出 | 正常弹出 |
| 斜杠命令执行 | 不执行(Enter 只用于中断) | 正常执行 |
| 主按钮 | Stop | Send |
| 附加文件按钮 | 可用 | 可用 |

关键取舍:Enter 继续承担「中断」,因此 turn 进行中不能靠回车发送——这正是
「必须先按 Stop 再发送」的产品意图,也避免用户误以为消息已进入当前回合。

## 实现

单文件改动 `desktop/src/components/MessageInput.tsx`:

1. `textarea` 的 `disabled` 去掉 `turnActive`,变为 `disabled || sending`。
2. `onKeyDown` 的 Enter 分支:当 `turnActive` 为真时,先于斜杠命令解析直接
   `onInterrupt()` 并返回——保证输入 `/clear` 之类也不会在回合中被执行。
3. `setPopupOpen` 的更新条件加上 `!turnActive`,回合中不弹建议框。

不改 `App.tsx`:其 `handleSend` 在 running 态本来就会走 `turn/interject`,
但 UI 上回合中根本不会触发发送,保持现状即是「必须 Stop」。

## 测试

`desktop/src/components/MessageInput.test.tsx` 新增:

- turn 进行中 textarea 不 disabled,且 `onDraftChange` 能被触发。
- turn 进行中按 Enter 调用 `onInterrupt` 且不调用 `onSend` / `onSlashCommand`。
- turn 进行中输入 `/` 不渲染弹窗。
- turn 进行中 Shift+Enter 不中断。

`npm test` 全绿。
