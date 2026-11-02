# 桌面端运行中插话与安全斜杠命令

日期：2026-11-03
状态：已批准（用户确认交互方案及书面规格）
分支：`feat/desktop-mid-turn-input`

## 目标与范围

桌面端有活跃 turn 时，用户可以继续提交文字，不必先 Stop。复用现有
`turn/interject`：消息立即投递给服务端，在 core 下一个可接收输入的模型轮次
生效，不取消正在执行的操作，也不等待整个任务结束才发送。手机端保留现有
“运行中先 Stop，再发送”的交互。

桌面端同时支持运行中的安全斜杠命令。命令只由客户端命令层解释，绝不作为
普通插话交给模型。

不扩展插话协议以支持附件；不修改 TUI、服务端调度、模型或权限配置控件的
既有生效规则；不新增独立的前端自动发送队列或可编辑消息队列。

## 已核对的现有实现

- `desktop/src/components/MessageInput.tsx`：运行中的桌面 Enter 调用
  `onInterrupt`，主按钮只显示 Stop；运行中不弹出斜杠建议。
- `desktop/src/App.tsx::send`：纯文字在 thread 状态为 running 时已有
  `turn/interject` 路径，并对 `-32012` / `-32013` 做一次方法切换。
- `yi-agent-rs/crates/yi-agent-app-server/src/rpc/turn.rs`：
  `turn/interject` 使用 `extract_prompt` 提取文字，不支持附件 block。
- `yi-agent-rs/crates/yi-agent/src/tui/queued.rs`：中途消息立即投递，列表仅记账
  尚未收到 core 回执的投递，不是等待整个任务结束的发送队列。
- `desktop/src/lib/slash.ts`：当前目录恰好为 clear、compact、fork、config、
  usage、help 六条，没有配置修改型命令。

本设计取代 `2026-10-21-composer-typing-during-turn-design.md` 中桌面端
“Enter 中断、发送必须先 Stop、运行中禁用全部斜杠命令”的约束；手机端不变。

## 交互规则

| 场景 | 桌面运行中 | 桌面空闲 | 手机运行中 |
| --- | --- | --- | --- |
| 普通文字 Enter | 提交插话，不中断 | 开启新 turn | 换行 |
| Shift+Enter | 换行 | 换行 | 沿用现状 |
| Send | 文字提交或处理斜杠命令 | 文字提交或处理斜杠命令 | 不显示 |
| Stop | 独立按钮，主动中断 | 不显示 | 保持原按钮 |
| 输入框编辑 | 保持可用，发送请求在途时沿用现有约束 | 沿用现状 | 沿用现状 |
| IME 候选确认 Enter | 只确认候选，不提交、不中断 | 同左 | 同左 |
| 斜杠建议 | 可打开 | 沿用现状 | 沿用现状，不新增入口 |

桌面运行中工具栏同时有独立的红色 Stop 与蓝色 Send；空闲只显示 Send。
Stop 不受发送请求的 sending 状态约束，避免在发送回包之前无法打断。
Send 在没有文字且没有附件、发送在途或输入区 disabled 时禁用。
空 Enter 不发送空消息，也不再作为停止快捷键。

手机端沿用现有 `isMobile` 判定，不改成按窗口宽度判断；窄桌面窗口仍是桌面
语义。手机空闲时 Send 的斜杠命令支持保持现状。

## 命令分类与草稿

| 命令 | 桌面运行中 | 原因 |
| --- | --- | --- |
| `/help [command]` | 执行 | 本地只读帮助 |
| `/usage` | 执行 | 查看当前会话用量，不改变上下文 |
| `/config` | 执行 | 读取当前会话配置，不修改配置 |
| `/clear` | 拒绝，提示等待结束或先 Stop | 清空上下文 |
| `/compact` | 拒绝，提示等待结束或先 Stop | 改写历史上下文 |
| `/fork [message]` | 拒绝，提示等待结束或先 Stop | 服务端要求源会话空闲 |
| 未知命令 | 显示未知命令帮助，保留输入 | 不能误发给模型，也不能自动中断 |

分类放在命令目录或其相邻纯逻辑中，供命令执行入口统一判断；输入框依据接受结果决定是否清稿。对运行中
可执行的命令使用明确白名单，新增命令未声明安全性时不得默认放行。

Enter、Send、建议弹窗选中项使用同一个命令提交路径。裸 `/` 不因建议框默认
选中了破坏性命令而执行；↑↓、Tab、Escape 及绝对路径识别沿用既有规则。
`/Users/me/src` 这类多斜杠路径仍是普通模型输入，不当作未知命令。

命令执行入口也检查活跃状态，避免只在键盘或建议框层做防护。被拒绝的命令
不发相应变更 RPC、不调用 `turn/interrupt`、不清草稿；未知命令同样保留输入。
允许执行的命令沿用现有成功处理；`/fork` 空闲时成功仍切换到副本并将参数写入
新会话草稿，不自动发送参数。

## 附件与失败边界

运行中提交带图片或文档的消息时，不调用只支持文字的 `turn/interject`，也不
尝试以 `turn/start` 硬起并行 turn。显示“当前任务进行中，附件需等待结束或先
停止后发送”之类的可操作提示，保留全部草稿和待发附件，不生成已发送气泡。
下一次任务空闲后用户可主动重新发送，不自动发送。

普通文字插话成功后清空本次提交的草稿；失败时沿用 App 的乐观消息回滚、
保留草稿和错误提示。状态竞争时保留现有一次方法自愈，不做无限重试。
现有 `turn/interjectionsReturned` 的回执与归还展示沿用，不承诺新增自动重投。

草稿和附件继续按 thread 隔离。切换会话时不得因上一会话请求完成而清掉新
会话草稿；若实现中发现提交回包清稿存在此窗口，应在本次相关提交路径内
以提交时 thread 与草稿快照保护它，不扩展为无关状态重构。

## 组件与数据流

1. `lib/slash.ts`：维护命令分类、未知命令判定及运行中可执行策略。
2. `components/MessageInput.tsx`：统一 Enter/Send/弹窗命令路径，桌面活跃时
   展示 Send + Stop，保留手机分支与 IME 保护。
3. `App.tsx`：命令执行入口做状态防护并返回是否可清稿的结果；发送入口在
   乐观回显前拒绝活跃 turn 的附件，纯文字继续复用已有 interject/start 流程。
4. 只在确有重复时抽出小型纯函数，不新增通用排队服务或更改 RPC 协议。

## 验收与测试

先写失败回归，再实现，最后跑完整前端测试和构建。

- `MessageInput.test.tsx`：桌面运行中 Enter/Send 提交但不调用 onInterrupt；
  Stop 仍可点击；空 Enter 无副作用；发送在途 Stop 不禁用；Shift+Enter 与
  IME 确认不提交；手机运行中只 Stop、Enter 换行。
- `slash.test.ts`：六条命令分类明确；只读白名单正确；未知命令不默认放行；
  原有绝对路径和分组一致性不回归。
- 输入框及 App 集成：运行中 help/usage/config 可执行且不发送模型消息；
  clear/compact/fork 不发变更 RPC、不停止且保留草稿；未知命令保留输入；
  Enter、按钮、弹窗三入口策略一致；空闲 fork 参数语义保持。
- `App.test.tsx`：运行中纯文字实际发送 turn/interject 而非 turn/interrupt；
  状态竞争仍可恢复；请求失败保留草稿；运行中带附件明确提示且不发任何
  turn RPC，保留附件与草稿；切换 thread 后回包不清错草稿。
- `mobileLayout.test.ts` 及既有相关测试：手机工具栏与单 Stop 入口无变化。

执行命令：

```sh
cd desktop
npx vitest run src/lib/slash.test.ts src/components/MessageInput.test.tsx src/App.test.tsx src/mobileLayout.test.ts
npx vitest run
npm run build
cd ../yi-agent-rs
cargo fmt --all
just ci
```

提交前 `just ci` 全绿；合并后在合并结果上再跑。同步更新
`docs/project-management/desktop.md` 中旧的桌面先 Stop 描述和相关行为条目，
新增条目时同步 README 索引计数，不把未完成实现标成已完成。

## 书面规格自检

- 无待定功能或占位实现；桌面与手机界限采用现有 isMobile，明确不按屏宽。
- 区分立即插话与整轮结束后的自动队列，避免误实现。
- 明确所有现有命令分类、未知命令、附件以及草稿保留规则。
- 服务端和 TUI 无变更；不引入运行中附件协议或配置修改语义扩展。
