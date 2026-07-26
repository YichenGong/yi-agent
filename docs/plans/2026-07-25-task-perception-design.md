# 任务执行感知改进设计

日期: 2026-07-25
状态: 已确认,待实现

## 目标

两条改进,都只针对 ratatui TUI 模式(默认模式;inline 为 legacy,不改):

1. **Token 用量可视化** — 用户能看到当前 LLM 调用的 prefill / decode token 实时计数,以及当前命令的执行耗时,状态栏常驻、30hz 刷新、token 数字线性插值平滑增长。让"任务在运行"变得可见、有趣。
2. **Bash 执行状态感知** — 用户能知道运行中的 bash 没卡住:spinner + 实时耗时 + 全屏弹窗查看 stdout/stderr 实时增量,并能手动 kill 进程。

## 总体架构

涉及的改动:

1. **状态栏(新增)** — `tui/statusbar.rs` 新模块,在 `tui/app.rs::run_loop` 中绘制于 history 与 input 之间(或 input 下方,实现时定)。数据源:
   - `UsageStats` 扩展为含 prefill / decode 分项 + 当前调用实时计数
   - 新增 `RunningTaskRegistry`(运行中 bash 任务 + 已完成列表)

2. **BashTool 流式改造** — `tools/shell/bash.rs::call` 不再阻塞收集完整结果。改为 spawn 进程,通过 `ToolEvent` 推送 stdout/stderr 增量与完成事件;agent loop 转发到 TUI。

3. **全屏弹窗(新增)** — `tui/bash_popup.rs` 新模块。Ctrl+P 打开列表态(选择运行中或已完成的 bash 任务),选中后切到详情态(全屏 stdout/stderr + spinner + 耗时 + exit code)。q/Esc 返回。

4. **tick 机制扩展** — `app.rs` 现有 tick 提升频率到 ~33ms(30hz)。状态栏在此 tick 上重绘;token 数字在线性插值下平滑增长;spinner 颜色渐变动画帧推进。

数据流:

```
LLM stream → ProviderEvent::Usage → UsageStats(当前调用) → StatusBar
BashTool spawn → stdout/stderr chunks → RunningTaskRegistry → StatusBar + BashPopup
tick(33ms) → 状态栏重绘 + 插值 + spinner 帧推进
```

## 状态栏内容与布局

底部单行,从左到右:

```
● bash 3.2s   prefill 1,234  decode 845   claude-opus-4
```

分段语义:

- **任务段**(只在有运行中任务时显示):`● <tool_name> <elapsed>` — 颜色渐变动画的小圆点 + 工具名 + 已运行时间。多个运行任务时显示 `● bash(2) 3.2s`(数量 + 最久那个的耗时)。
- **token 段**:`prefill <n>  decode <m>` — 当前这一轮 LLM 调用的实时计数,数字用千分位逗号。任务结束后不立即清零,保留最终值直到下一轮 LLM 调用开始时清零。
- **model 段**(已有):当前 model 名,灰色。
- **分隔**:段间两空格,段内单空格。颜色:运行中=黄,完成=绿,失败=红;token 数字=青。

空状态(无运行中任务、不在 LLM 调用中、只有 model 段):只显示 `claude-opus-4`(灰色),状态栏视觉存在感低。

token 插值细节:

- 收到 `ProviderEvent::Usage { input_tokens, output_tokens }` 时更新"目标值"
- tick(33ms)时把"当前显示值"线性逼近"目标值",每帧增加 `min(remaining, max(1, target/30))`(约 1/30 的差距),让数字平滑增长
- 1 秒内无新事件,认为该轮 LLM 调用结束,停止插值(保留最终值)

耗时插值:bash 任务运行时,33ms tick 上直接重算 `now - start_time`,无需插值。

## BashTool 流式改造与超时策略

### 超时策略(替换原 120s 硬超时)

- BashTool input schema 新增 `expected_timeout_sec: u32`,默认 120
- 进程运行超过 `expected_timeout_sec × 1.5` 时:
  - **若期间有新 stdout/stderr 输出** → 继续运行(有输出说明在干活)
  - **若期间完全无输出** → 判定卡住,发送 `ToolEvent::Timeout` 后杀进程
- 一旦有新输出到达,"无输出计时器"重置(每次有输出后重新等 `expected × 1.5` 的无输出窗口)
- 状态栏与弹窗在超过 `expected_timeout_sec` 时显示"超出预期"警告(黄色),超过 `×1.5` 且无输出时显示"疑似卡住,终止中"(红色)

### ToolEvent 形状(实现时按现有定义对齐)

```
ToolEvent::OutputDelta { stream: Stdout|Stderr, text: String }
ToolEvent::Exit { code: i32 }
ToolEvent::Timeout                    // 触发了无输出超时
ToolEvent::Truncated { stream, skipped_bytes }  // 单流超过 100KB
```

### BashTool 内部

- `tokio::process::Command::new("sh").arg("-c").arg(cmd).stdout(piped).stderr(piped).spawn()`
- `tokio::select!` 同时驱动 stdout/stderr reader 和一个"无输出 watchdog" timer
- 每收到一块输出就重置 watchdog
- watchdog 触发 → 发 Timeout + kill child
- 仍保留 100KB 截断,达到上限时发 Truncated 后停止该流 delta

### agent loop 与 RunningTaskRegistry

- `AgentEvent` 新增 `ToolOutputDelta { tool_call_id, stream, text }` 和 `ToolExit { tool_call_id, code }`
- `accumulate_provider_stream` 之外,tool 执行路径也需要 streaming accumulate
- Exit 后由 agent loop 拼装最终 ToolResult 文本(`exit: {code}\nstdout:\n{...}\nstderr:\n{...}`)给 LLM
- 新增 `RunningTaskRegistry`(`tui/state.rs` 或 `app.rs`):`HashMap<ToolCallId, TaskState>`
- `TaskState = { cmd, start_time, end_time?, exit_code?, stdout: Vec<u8>, stderr: Vec<u8>, status: Running|Done|Failed|Timeout, expected_timeout_sec }`
- 收到 ToolCall 事件插入;OutputDelta 追加;Exit 更新状态
- 截断策略:每个流保留最后 64KB 用于全屏弹窗显示,避免长输出吃内存

## 全屏弹窗 + Ctrl+P 交互

两态弹窗:列表态 / 详情态。Ctrl+P 切换。

触发:全局键 Ctrl+P。有运行中或已完成 bash 任务时打开列表;没有则不开窗(或状态栏短暂提示"无 bash 任务",实现时定)。

### 列表态

```
┌─ bash tasks ──────────────────────────┐
│ ●  bash  ls -la          3.2s   running│
│ ✓  bash  cargo build     12.4s  done   │
│ ✗  bash  npm install      8.1s  failed│
└────────────────────────────────────────┘
```

- 每行:`状态符 + tool + 命令(截断) + 耗时 + 状态`
- 列表按开始时间倒序(最新在上)
- 上下方向键选择,Enter 进入详情,Esc/q 关闭弹窗回主界面
- 状态符颜色:running=黄(带颜色渐变动画的圆点),done=绿,failed=红,timeout=红

### 详情态(全屏)

```
┌─ bash ● running 3.2s (expected 120s) ───────┐
│ $ ls -la                                    │
│                                             │
│ stdout:                                     │
│ total 48                                    │
│ drwxr-xr-x  6 user staff  192 Jul 25 10:00 .│
│ ...                                         │
│                                             │
│ stderr:                                     │
│ (empty)                                     │
│                                             │
└─────────────────────────────────────────────┘
 [q] back  [k] kill  [↑↓] scroll
```

- 顶栏:状态符 + 状态 + 耗时 + (expected 提示);超出 expected×1.5 显示"超出预期"红字
- 中部:stdout / stderr 顺序排列(实现简单)
- 底栏:操作提示
- 实时刷新:33ms tick,stdout/stderr 滚动显示最新内容(tail -f 行为)
- **`k` 杀进程 + 二次确认**:按 `k` 后弹窗内显示二级确认框(`[y] confirm   [n/esc] cancel`),`y` 确认发 kill signal,留在详情态看进程结束;`n` 或 Esc 取消。进程已完成/失败时 `k` 不显示。
- **`q` 或 Esc**:返回列表态(不杀进程)
- 完成后仍可查看(状态符变 ✓/✗),不再实时刷新

滚动:详情态下 stdout/stderr 超过一屏时,默认锁定到底部(看最新输出);↑↓ 手动滚动时解锁,按 `f` 或拉到底重新锁定。

## tick 机制与刷新

- `app.rs` 现有 tick 频率提升到 ~33ms(30hz)
- 33ms tick 上做:
  - 状态栏重绘
  - token 数字线性插值(逼近目标值)
  - spinner / 颜色渐变动画帧推进
  - bash 任务耗时实时计算 `now - start_time`
- 不引入独立线程,所有状态变更仍由事件驱动(ProviderEvent / ToolEvent / tick),单线程 tokio runtime 内完成

颜色渐变 spinner:33ms tick 上推进色相值(HSL hue += 8°/帧,~1.5s 一圈),渲染时取当前 hue 画圆点。ratatui 支持真彩色。

## 实现步骤拆分(按依赖顺序)

1. **ToolEvent 扩展 + BashTool 流式改造**
   - 读 `tools/registry.rs` / `core/tool.rs` 确认 ToolEvent 现状
   - 新增 `OutputDelta` / `Exit` / `Timeout` / `Truncated` 事件
   - `BashTool::call` 改为 spawn + tokio::select + stdout/stderr reader
   - 实现 `expected_timeout_sec` + 无输出 watchdog 超时

2. **agent loop 转发 + ToolResult 拼装**
   - `AgentEvent` 新增 ToolOutputDelta / ToolExit
   - Exit 后拼装最终 ToolResult 文本给 LLM
   - 保留原有 ToolCall/ToolResult 的 history cell 行为不变

3. **RunningTaskRegistry**
   - 新增 `tui/state.rs`(或放 `app.rs`),`HashMap<ToolCallId, TaskState>`
   - 接收 AgentEvent,更新 TaskState
   - 截断策略:每流保留最后 64KB

4. **UsageStats 扩展**
   - 拆分 prefill / decode 计数
   - 当前调用实时值 vs session 累计(状态栏只用当前调用)
   - 任务结束保留最终值,下一轮 LLM 调用开始时清零

5. **状态栏渲染**
   - 新增 `tui/statusbar.rs`
   - 三段布局(任务 / token / model)
   - tick 驱动插值 + 渐变 spinner

6. **Ctrl+P 弹窗 — 列表态**
   - 新增 `tui/bash_popup.rs`
   - 列出 registry 中所有任务
   - 上下选择 + Enter 进入详情 + Esc/q 关闭

7. **Ctrl+P 弹窗 — 详情态**
   - 全屏 stdout/stderr + spinner + 耗时
   - 实时 tail -f 行为 + 滚动锁定
   - `k` 杀进程 + 二次确认
   - 完成后可查看

8. **键位绑定**
   - `tui/app.rs` 加 Ctrl+P 全局键,仅在 TUI 模式有效
   - 与 Ctrl+O 折叠不冲突

每步可独立测试(1-3 数据层,4-8 UI 层)。

## YAGNI 砍掉的

- 多任务统计聚合(只显示最久那个的耗时,不做总和)
- token 历史曲线图
- 弹窗内过滤/搜索功能
- 主题切换
- inline 模式同步改进(legacy,不改)
