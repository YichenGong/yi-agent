# 手机端设置界面：全屏页 + 顶部 Tab 条 — 设计

日期：2026-10-17
状态：设计已确认（方案 A：全屏页）
分支：`fix/mobile-settings-layout`
范围：仅前端 `desktop/`（`SettingsDialog.tsx`、`SettingsModelsTab.tsx`、两处测试、`index.css`、`App.tsx` 传参、`docs/project-management/desktop.md`）

---

## 1. 问题与已核实事实

用户反馈：手机端设置界面「看起来不太自然，设置内容的区域过于小」。

核实后的事实（真实 Chrome 无头 + 真实 `index.css`，`Emulation.setDeviceMetricsOverride`
390×844 / deviceScaleFactor 3，测的是**当前**实现）：

| 元素 | 实测宽度 | 说明 |
| --- | --- | --- |
| 弹窗外框（`w-[70vw] max-w-3xl`） | **273px** | 390 × 0.7 |
| 左侧 Tab 栏（`w-40`） | **160px** | 写死像素，不随视口缩放 → 占 59% |
| 内容面板（剩余） | **111px** | `273 - 160 - 2`（两侧各 1px 边框） |

根因是两处固定量的叠加：

1. `w-[70vw]` 在 390pt 上只有 273px——弹窗本可以更宽，却在窄屏自废武功。
2. 左侧栏是写死的 `w-40`（160px）。桌面 70vw 有 500px+，160px 只占三成，观感正常；
   手机上它瞬间吃掉近六成。

连带缺陷（逐 Tab 实测，`panelClientWidth` 恒为 111px）：

| Tab | 内容实际需要 | 横向溢出 | 症状 |
| --- | --- | --- | --- |
| 通用 | 115px | +4px | 「深色 / 浅色」按钮越出面板右缘 |
| 远程访问 | 139px | +28px | 「撤销 iPhone 15 Pro」按钮落在面板外 |
| 插件 | 294px | +183px（29 个元素越界） | 星期按钮、输入框成片出界 |
| 模型 | **463px** | **+352px（30 个元素越界）** | 5 列表格被裁，只剩前两列 |

溢出**无法补救**：面板是 `min-h-0 flex-1 overflow-y-auto`（只允许纵向滚动），横向溢出
既不显示滚动条也滚不到，右侧内容永久不可达——`docs/bug-list.md:11`「看不到每行的删除
按钮」正是同一个坑的旧账。

另有一处独立缺陷：面板内文本输入框字号为 **14px**，低于 iOS 触发聚焦自动放大的 16px
下限。`index.css:315` 目前只给侧栏搜索框钉了 16px，设置页未覆盖；手机上点输入框会被
系统放大到不贴合屏幕（`lockZoom` 只能禁手动捏合，挡不住这条自动放大）。

## 2. 决策（方案 A：全屏页）

1. **手机端把设置从「居中弹窗」改为「全屏页」**（`fixed inset-0`，`z-50` 不变）。
   实测内容区从 **111px → 390px**（约 3.5 倍）。
2. **Tab 导航从左侧竖栏改为顶部横条**，每格 `flex-1`。实测 320/360/375/390/430pt
   五档 `tablist.scrollWidth == clientWidth`（零溢出），最窄 320pt 每格仍有 80px，
   「远程访问」四字单行放得下。
3. **遮罩在手机端隐藏**：全屏页没有浮层，遮罩无意义。关闭走右上角按钮（44×44）或
   Escape。桌面端遮罩与点击关闭行为不变。
4. **「模型」Tab 窄屏改卡片**：表格固有 463px，与外壳无关，必须单独改造——窄屏下
   每条模型渲染成一块（名称作标题，其余字段纵向排布），桌面仍是 5 列表格。
5. **面板内输入控件字号钉到 16px**（`[data-mobile="true"]` 前缀），堵住 iOS 聚焦自动
   放大。
6. **桌面端（≥768px 且非手机客户端）逐字节不变**：全部改动落在 `isMobile` 条件渲染
   与 `[data-mobile="true"]` CSS 前缀下。

选 A 而非 B（`w-[92vw] h-[88vh]` 近全屏弹窗）的理由：A 拿到 390px 内容宽、B 只有
357px；且遮罩与圆角在窄屏上只剩占地方的作用没有视觉收益。手机上设置本质是一个独立
目的地，而非浮层。

## 3. 分层设计

### 3.1 `SettingsDialog`（`desktop/src/components/SettingsDialog.tsx`）

新增可选 prop `isMobile?: boolean`（默认 `false`，与 `ThreadSidebar` / `MessageInput` /
`ThreadDetailPanel` 同形）。`App.tsx` 把已有的 `isMobile` 传下来（`App.tsx:198` 已调
`useIsMobile()`，故 `<html data-mobile>` 在设置打开时已就位）。

- **挂点类**：根 shell 加 `settings-shell`、导航加 `settings-nav`、面板加
  `settings-panel`。三个类**无条件渲染**（两种平台都在），但桌面没有任何规则选中它们，
  故**渲染结果**逐字节不变；手机端的外观覆写写在 `index.css`（`[data-mobile="true"]`
  前缀），与 `.app-statusbar`（`index.css:242`）同源——桌面不匹配任何一条。
- **`aria-orientation` 随方向切换**：桌面 `vertical`、手机 `horizontal`。这是语义，
  CSS 表达不了，故由 `isMobile` 驱动。
- **方向键逻辑不改**：现有 `onTablistKeyDown` 已同时处理上下左右与 Home/End
  （`SettingsDialog.tsx:93`），两种朝向都成立。
- **roving tabIndex / `aria-controls` / `aria-labelledby` / 打开落焦 / 关闭还焦 /
  Escape 关闭**：全部保持现状，不因布局改动而变。
- 手机端关闭按钮提到 44×44（Apple HIG；项目在抽屉按钮 `size-11` 已有先例）。

`index.css` 新增（最终值）：

```css
/*
 * 手机端设置：居中弹窗 → 全屏页。
 *
 * 桌面是「70vw 居中浮层 + 160px 左侧竖栏」；390pt 上 70vw 只有 273px，左侧栏写死
 * 160px 就吃掉 59%，内容面板只剩 111px，且横向溢出滚不到（面板只允许纵向滚动）。
 * 手机端改成整屏页面，导航移到顶部横排，内容拿到全部宽度。桌面不匹配
 * `[data-mobile="true"]`，逐字节不变。
 */
[data-mobile="true"] .settings-shell {
  position: fixed;
  inset: 0;
  display: flex;
  flex-direction: column;
  width: auto;
  max-width: none;
  height: auto;
  border: 0;
  border-radius: 0;
  /* 底部安全区收在滚动容器上（见下），这里不铺，免得滚动条区域也留白。 */
  padding-bottom: 0;
}

/* 导航：竖栏 → 顶部横条，每格等宽。320pt 上每格仍有 80px，最长的
   「远程访问」单行放得下（实测 320/360/375/390/430pt 均零溢出）。 */
[data-mobile="true"] .settings-nav {
  flex-direction: row;
  width: auto;
  flex-shrink: 0;
  border-right: 0;
  border-bottom: 1px solid var(--line);
}
[data-mobile="true"] .settings-nav > [role="tab"] {
  flex: 1 1 0;
  white-space: nowrap;
  text-align: center;
  padding-top: 12px;
  padding-bottom: 12px; /* 单格高 ≈ 44px，达到 Apple HIG 触控下限 */
}

/* 全屏页没有浮层，遮罩无意义——隐藏它（关闭走右上角按钮或 Escape）。
   属性选择器复用既有的 `data-settings-backdrop`，不新加类名；jsdom 无
   `data-mobile`，该规则不生效，「点击遮罩关闭」用例不受影响。 */
[data-mobile="true"] [data-settings-backdrop] {
  display: none;
}

/* 面板：全屏页里是唯一的滚动容器，底部安全区补在它身上（与侧栏 `aside` 同一理由，
   fixed 元素脱离 `body` 的内边距），滚到底时最后一行不被 Home Indicator 压住。 */
[data-mobile="true"] .settings-panel {
  padding-bottom: env(safe-area-inset-bottom, 0px);
  overscroll-behavior: contain;
}
```

安全区：全屏页是 `fixed`，脱离 `body` 的内边距，必须自己补——顶部
`env(safe-area-inset-top)` 落在头部栏，底部 `env(safe-area-inset-bottom)` 落在**滚动
容器**上（落在 shell 上会让滚动条区域留白）。与侧栏 `aside` 的做法一致。

### 3.2 「模型」Tab（`desktop/src/components/SettingsModelsTab.tsx`）

表格固有 463px，外壳换掉也救不了。按 `isMobile` 条件渲染两种呈现：

- 桌面：现有 5 列表格**原样保留**（搬进一个子组件，JSX 与类名不变）。
- 手机：每条模型一张卡片——名称作标题，`API 格式 / 模型标识 / API 地址 / 密钥` 纵向
  排布（复用现有的「字段名 + 值」两行式），「编辑 / 删除」按钮跟在卡片下方并给足触控
  高度。

字段标签（「API 格式」等）在卡片形态下必须显式渲染；表格形态下它们由 `<th>` 承担。
两种形态共用同一份数据与同一套回调，不复制业务逻辑。

### 3.3 输入控件字号（`desktop/src/index.css`）

```css
[data-mobile="true"] .settings-panel input,
[data-mobile="true"] .settings-panel select { font-size: 16px; }
```

与 `index.css:315` 的搜索框规则同源、同理由（iOS 自动放大下限）。桌面不匹配，逐字节
不变。

### 3.4 不改的部分

`TABS` 表与 Tab 语义、各 Tab 的业务逻辑与 RPC 接缝（`remoteCall` / `pluginCall` /
`modelCall`）、`SettingsGeneralTab` / `SettingsRemoteTab` / `SettingsPluginsTab` 的
内部结构（插件 Tab 在 `docs/bug-list.md:11` 已改成 flex-wrap 行布局，外壳换掉即自愈）、
`SettingsRemoteTab` 的二维码与配对码渲染、后端与协议——全部不动。

## 4. 边界与不变量

- **桌面零回归**：未传 `isMobile`（默认 `false`）时，除三个无样式命中的挂点类外，
  `SettingsDialog` 渲染出的 DOM 结构与类名与现在完全一致；`[data-mobile="true"]`
  规则桌面不匹配。
- **既有 13 条 `SettingsDialog` 测试全绿**：它们都不传 `isMobile`，走桌面分支。
- **`[data-mobile]` 由 `useIsMobile` 唯一设置**（`App.tsx` 已调用），`SettingsDialog`
  不重复调用该 hook（避免重复 effect 与双写 `dataset`）。
- **遮罩在 jsdom 测试里仍存在**：jsdom 无 `data-mobile`，`display: none` 规则不生效，
  「点击遮罩关闭」用例不受影响。
- **手机端键盘可达性不回退**：Tab 横排后方向键、Home/End、Escape、关闭按钮均可用；
  焦点仍落在选中 Tab 上并随选区移动。
- **极窄屏兜底**：320pt 下 Tab 标签单行不折（实测 80px/格）；若未来新增更长的 Tab
  名，`whitespace-nowrap` + `overflow-x-auto` 兜底，不横向撑破页面。

## 5. 判据

- `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/components/SettingsModelsTab.test.tsx src/mobileLayout.test.ts`
  - `SettingsDialog.test.tsx` 新增：手机端（`isMobile`）时 `aria-orientation="horizontal"`、
    存在 `settings-shell` 挂点类、关闭按钮可点；桌面端仍为 `vertical`。
  - `SettingsModelsTab.test.tsx` 新增：`isMobile` 时不渲染 `<table>`（改卡片）、桌面时
    仍渲染表格。
  - `mobileLayout.test.ts` 新增静态门禁（读**真实** `index.css` 与组件源，与现有
    `.app-statusbar` / `MarkdownText` 两条同源）：挂点类与 `[data-mobile="true"]` 覆写
    规则成对存在，删任一侧即红；输入控件 16px 规则存在。
- `cd desktop && npx vitest run`（全量，当前 1047）/ `npx tsc --noEmit` / `npm run build`
- 真实 Chrome 复测：390/375/320pt × 四个 Tab，`panel.scrollWidth <= panel.clientWidth`
  （零横向溢出）；桌面 800 / 1280px 量测与改动前逐像素一致。