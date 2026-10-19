# 状态栏 token 用量翻滚数字设计

## 状态

设计已由用户确认。

## 背景

桌面端状态栏右下角显示 token 用量 `{input} in / {output} out`
（`desktop/src/components/StatusBar.tsx:49`）。数字每次变化都是整串替换，观感上
是「一下子跳到新数字」，缺少过程感。

数据来源与变化规律（决定了动效必须支持哪些情况）：

- 每个会话的 `Usage` 来自 `thread/tokenUsage/updated` 通知
  （`desktop/src/lib/session.ts:594`）。
- 通知携带的是**本轮**用量快照：服务端在每轮开始时清零
  （`yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:116` 的 `set_turn`），
  因此轮次切换时数字会**下降**。
- 一次 provider 调用只在 `message_start`（input / cache）与
  `message_delta`（output）两个时刻各推一次，不是逐 token 连续推送；两次推送
  之间数字静止。

## 目标

1. 状态栏的 input / output 数字变化时，以逐位竖向翻滚（odometer）呈现，而不是
   瞬间跳变。
2. 连续推送到达时从当前位置继续滚动，不重播、不回跳。
3. 递增与递减都正确（轮次切换时数字会下降）。
4. 不改变状态栏布局高度、不改变按钮行为与无障碍语义。
5. 尊重系统「减少动态效果」设置。

## 非目标

- 不改 `UsagePanel` 浮层里的明细（展开后的静态读数）。
- 不改状态栏右侧的子 agent 计数。
- 不加千分位分隔符，不把大数缩写成 `1.2k`。
- 不改服务端用量口径或推送频率。

## 方案

逐位数字轮：每个数字位是一条竖向数字带，带内重复 `0-9`，外面套一个只显示一行
的裁切窗口；数字变化时数字带沿 Y 轴滚到目标位。

### 滚动规则

对每个数字位，按「从右往左」的位数对齐（个位为第 0 位）：

- 方向：由整个数值的增减决定（`next > prev` 为向上，反之为向下）。
- 步数：把该位的新旧数字差按 10 取模到滚动方向。
  例：递增时 `9 -> 0` 走 1 步、`0 -> 9` 走 9 步；递减时 `0 -> 9` 走 1 步。
  这正是机械里程表的进位行为。
- 新旧数字相同的位不移动，时长与延迟均为 0。

### 时长与错峰

- 单步时长 `BASE_MS = 260`，每步加 `PER_STEP_MS = 45`，上限 `MAX_MS = 720`。
- 个位立即起步；每往左一位追加 `STAGGER_MS = 20` 延迟，最多累计到 6 位
  （`MAX_STAGGER_DIGITS = 6`），即最大额外延迟 100ms。
- 缓动 `cubic-bezier(0.16, 1, 0.3, 1)`：快起慢收，停下时有定格感。

### 连续更新

动画由 CSS `transition` 驱动 `transform: translateY(...)`，而不是 JS 逐帧改样式。
新数据在动画途中到达时，浏览器从元素**当前实际位置**继续过渡到新目标，因此不会
跳回起点重播。这是「一直在变」的手感来源。

### 数字带与位置归一

- 数字带渲染 `0-9` 三份（30 个字符），窗口高 `1em`、宽 `1ch`。
- 位置以「带内字符索引」表示，初始放在第二份的对应位（`10 + digit`），向上、
  向下各留 9 步余量。
- 每次过渡结束后把位置归一到 `[10, 19]`（同一字符，视觉零变化），并把该位的
  时长置 0，避免长时间运行后索引漂移出数字带。

## 组件划分

- `desktop/src/lib/rollingDigits.ts`：纯函数，无 DOM。
  - `splitDigits(value: number): number[]`：拆位，最高位在前；负数取绝对值；
    `0` 返回 `[0]`。
  - `digitMove(from: number, to: number, direction: 1 | -1 | 0)`：返回
    `{ steps, direction }`；数字相同或整体方向为 0 时步数为 0。
  - `digitDurationMs(steps: number): number`：步数到时长，0 步为 0。
  - `digitDelayMs(indexFromRight: number): number`：低位到高位的错峰延迟。
- `desktop/src/components/RollingNumber.tsx`：渲染数字轮，维护每位的目标位置。
- `desktop/src/components/StatusBar.tsx`：把用量按钮里两段数字换成
  `RollingNumber`，文案与按钮行为不变。
- `desktop/src/index.css`：新增 `.rolling-number` / `.rolling-digit` /
  `.rolling-strip` 三个挂点类。

## 边界处理

- 首次挂载、`usageKey` 变化（切换会话）、数据从无到有：直接显示终值，不播动画。
  `StatusBar` 新增可选 `usageKey`，由 `App.tsx` 传入当前会话 id。
- 位数变化（如 `99 -> 100`）：新增的高位直接出现在终值，已有位正常滚动。
- 系统「减少动态效果」：在 JS 侧读 `matchMedia("(prefers-reduced-motion: reduce)")`，
  命中则所有位时长置 0，直接跳到终值（与 `desktop/src/components/ThreadSidebar.tsx`
  涟漪动效的判定方式一致）。

## 可访问性与布局

- 数字轮根节点 `aria-hidden="true"`；按钮内另放一段 `sr-only` 完整文本
  `"{input} in / {output} out"`，读屏与文本查询拿到的仍是完整数字。
- 按钮原有的 `aria-label="Token usage details"`、`aria-expanded`、
  `aria-controls` 与点击行为保持不变。
- 数字窗口高 `1em`、宽 `1ch`，配合按钮的 `inline-flex items-center` 对齐；
  状态栏行高与高度不变。
- 手机端既有规则 `[data-mobile="true"] .app-statusbar { white-space: nowrap }`
  继续生效；数字轮自身不产生空白，不会引入折行。

## 测试

- `desktop/src/lib/rollingDigits.test.ts`：拆位、进退位步数、时长上下限、错峰延迟上限。
- `desktop/src/components/RollingNumber.test.tsx`：终值渲染、未变化的位不移动、
  递减方向正确、`usageKey` 变化不带动画、减少动态效果时时长为 0。
- `desktop/src/components/StatusBar.test.tsx`：用量仍渲染为完整文本，按钮语义不变。

验证命令：

```bash
cd desktop && npx vitest run \
  src/lib/rollingDigits.test.ts \
  src/components/RollingNumber.test.tsx \
  src/components/StatusBar.test.tsx \
&& npx tsc --noEmit && npm run build
```

## 考虑过但未采用

- 数值补间（count-up）：实现更省，但不是「翻滚」，个位高频刷新会糊成一片。
- 整串交替（旧串上移淡出、新串下移淡入）：只有一次过渡，看不到逐位逼近的过程。