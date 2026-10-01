# Spec 3：桌面端建卡入口

日期：2026-10-01
状态：已实施（Spec 3，桌面端 32 文件 / 275 测试通过）

## 1. 目标

桌面端原先**能看、能开关，但不能建卡**：`desktop/src/lib/superpowersKanbanSwitch.ts` 里的 `enqueueBoardCard()` 是死代码。本 spec 补上了入口——看板面板内的「加入看板」按钮（组件 `SuperpowersKanbanEnqueue`）。

## 2. 交互

- 位置：看板面板内的一个「加入看板」按钮。
- 点击后：**原生文件选择器选两次**——先选 spec 文件，再选 plan 文件（复用已有的 `@tauri-apps/plugin-dialog`，`App.tsx` 的 `pickDirectory` 同款用法）。
- 提交：调用入队通道（Spec 4 之前为 `superpowers-kanban/enqueue` / 之后为 `plugin/query` 的 `enqueue` 方法），参数为两个绝对路径。
- 反馈：
  - 成功 → 提示"已投递，等待插件校验"（**不**说成"已加入队列"：投递要等插件下一个 tick 才落进 `board.json`，Spec 2 的 `list` 把它显示为 `pending`）。
  - 文件对不合法（缺失、同名）→ 显示被拒原因（由插件侧 `validate_promotion` 给出）。
  - 插件未安装（Spec 4 之后）→ 显示"插件未安装"。

## 3. 边界

- 只选**文件**，不选目录；取消选择即中止，不产生请求。
- 不做拖拽、不做路径手输（YAGNI）。
- 不在此入口生成 spec/plan——与 Spec 2 的 skill 职责一致：**只投递**。

## 4. 依赖关系

本 spec 的落点会随 Spec 4 变化：

- 若在 Spec 4 **之前**实施：调 `superpowers-kanban/enqueue` RPC。
- 若在 Spec 4 **之后**实施：调 `plugin/query`，`plugin: "superpowers-kanban"`、`method: "enqueue"`。

overview 建议的实施顺序把本项放在 Spec 4 之前，故先按前者实现，Spec 4 时改道。

## 5. 测试重点

1. 点按钮 → 选择两个文件 → 发出一次入队调用（参数为两路径）。
2. 取消选择 → **不**发出调用。
3. 入队失败 → 面板显示可读错误，不崩。
