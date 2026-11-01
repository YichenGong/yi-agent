# 字号档位与按平台隔离的设置参数 设计

**日期**: 2026-11-02
**状态**: 待实现

**相关代码**:
- `desktop/src/index.css`（Tailwind 主题层字号变量、移动端 16px 兜底）
- `desktop/src/lib/theme.ts`（主题偏好：localStorage 首屏缓存 + 权威值在 preferences.json）
- `desktop/src/hooks/useSettingsState.ts`（本机偏好关注点）
- `desktop/src/components/SettingsGeneralTab.tsx`（「通用」页）
- `desktop/src/components/SettingsDialog.tsx`（Tab 表与接线）
- `desktop/src/App.tsx`（握手、首屏 `ui/settings/read` 采纳、通知跟随）
- `yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs`（preferences.json 读写）
- `yi-agent-rs/crates/yi-agent-app-server/src/rpc/ui.rs`（`ui/settings/read|write`）

---

## 1. 问题

两件相关但独立的事。

### 1.1 手机端字号偏大

iOS 与桌面共用同一份前端产物，字号全部来自 Tailwind v4 的
`--text-xs/sm/base/lg`（rem 值）。同一套尺寸在 390pt 宽的手机上偏大：行数变少、
可读性反而下降。用户要求「适当小一点」。

### 1.2 设置参数没有按平台区分

`preferences.json` 是**单个全局文件**，桌面 sidecar 的 `workdir` 是实例 home 的
父目录（`desktop/src-tauri/src/bridge.rs:433`），落点恒为
`<实例 home>/preferences.json`。iOS 是配对后的 ws 客户端，写的是**那台桌面机器**
上的同一个文件。于是主题、值守这类偏好两个平台共用一份，互相覆盖。

更具体的一处错位：`SettingsGeneralTab` 只要有 props 就渲染「后台值守（开机自启）」，
而 `App.tsx` 无条件传值。iPhone 上因此能拨一个会去**桌面主机**装/卸登录项的开关
——它对这个手机端没有任何意义。

---

## 2. 目标与非目标

**目标**

1. 新增「字号」档位（小 / 标准 / 大），iOS 默认「小」，桌面默认「标准」。
2. 两个平台的字号偏好**各自独立保存**，互不覆盖。
3. 「通用」页只显示当前平台需要的参数：iOS 不显示「后台值守」。

**非目标**

- 不做逐元素字号微调，只缩放任一比值的整条尺度。
- 不改主题的存储与语义（主题仍按现有方式单值保存；本次只保证字体这一项按平台隔离，
  并把「参数按平台隔离」的机制立起来）。
- 不做字号与系统「辅助功能 → 文字大小」的联动（WKWebView 下 `text-size-adjust`
  会把两者叠乘，反而不可控）。

---

## 3. 字号如何缩放

### 3.1 只覆盖字体尺度变量

`--text-*` 由 Tailwind 在 `@layer theme` 里定义；`index.css` 中未分层的 `:root`
规则优先级高于任何 layer，因此可以就地重定义，不改任何工具类：

```css
:root {
  --font-scale: 1;
  --text-xs: calc(0.75rem * var(--font-scale));
  --text-sm: calc(0.875rem * var(--font-scale));
  --text-base: calc(1rem * var(--font-scale));
  --text-lg: calc(1.125rem * var(--font-scale));
}
html[data-font-scale="small"] { --font-scale: 0.9375; }
html[data-font-scale="large"] { --font-scale: 1.125; }
```

档位因子：

| 档位 | 因子 | 正文（`text-base`） | 次要（`text-sm`） |
|---|---|---|---|
| `small` | 0.9375 | 15px | 13.125px |
| `standard` | 1 | 16px | 14px |
| `large` | 1.125 | 18px | 15.75px |

**为什么只动字号**：间距、圆角、44px 触控目标走的是 `--spacing` 等另一套变量，
不受影响；行高是单位无关的比值（`calc(1.5 / 1)`），缩放后行距比例自动保持。
所以这是「字变小、版面不变」。

### 3.2 markdown 正文要单独接上

聊天正文由 `MarkdownText` 的 `.prose` 承载，而 typography 插件把 `.prose` 的
字号**硬编码**为 `font-size: 1rem`，不引用 `--text-base`。不补这一条，最需要
变小的聊天区反而纹丝不动。补：

```css
html[data-font-scale] .prose {
  font-size: var(--text-base);
}
```

`.prose` 内部标题、列表、代码的尺寸都是相对 `em` 的，跟着自动缩放；行高
`1.75` 是比值，一并保持。选择器特异度 (0,2,1) 高于 `.prose` 的 (0,1,0)，
与打包顺序无关。

`applyFontScale` **总是**写 `data-font-scale`，包括 `standard`。桌面停在标准档时
这条规则命中、`--font-scale` 为 1，`var(--text-base)` 恰好等于原值 `1rem`——
计算值逐字节相同，观感不变。这样取值只有一条路径（属性总是存在），不必再维护
「未设属性」与「设为标准」两种等价状态。

### 3.3 保留 iOS 的 16px 输入框兜底

`index.css:323`（侧栏搜索框）与 `index.css:433`（设置面板输入控件）把输入框字号
钉在 16px，因为 iOS 在聚焦字号 < 16px 的输入框时会自动放大页面，而 `lockZoom`
只能拦用户的手动捏合缩放。**这两条不改**：小档正文 15px 与 16px 输入框只差 1px，
目视一致；若跟着缩放，输入框反而比正文明显更大，且失去防放大能力。

---

## 4. 按平台隔离的存储

### 4.1 键名

沿用 `preferences.json` 现有的扁平键风格（`theme` / `relay_url` / `tag_names`），
新增两个**平级**键：

| 键 | 含义 | 默认 |
|---|---|---|
| `font_scale_desktop` | 桌面客户端的字号档位 | `standard` |
| `font_scale_ios` | 配对手机端的字号档位 | `small` |

客户端只写自己那一半：`isMobile` 为真写 `font_scale_ios`，否则写
`font_scale_desktop`。两个键互不影响，桌面改了字号不会动手机端的值，反之亦然。

合法值只有 `"small" | "standard" | "large"`。

### 4.2 读写契约

- `ui/settings/read` 同时返回两个键，缺省为 `null`（= 未设置，由客户端取平台默认）。
- `ui/settings/write` 接受其一或两者；**非字符串或非法值一律 `invalid_params` 失败**，
  与 `theme` 的校验口径一致（先校验再落盘，非法值不会把同一请求里的其它键写一半）。
- 读失败 / 坏文件 / 缺键：不报错、不阻断，返回 `null` 让客户端回落默认。与主题同理。
- 落盘走 `settings_store` 已有的读-改-写 + 原子替换，保留无关键。

### 4.3 为什么不用嵌套对象

`{"font_scale": {"ios": "small", "desktop": "standard"}}` 语义更整齐，但
`settings_store` 的既有写路径是 `set_object_value(key, Value)`（顶层键读-改-写）。
嵌套要新增一条「合并子键」的写路径，且与 `tag_names` 已有的「对象型键」语义撞车
（那个是覆盖式合并单个键）。两个平级键在磁盘上的可读性也更好，排查时一眼看到
两个平台各自的值。

---

## 5. 前端接线

### 5.1 新模块 `lib/fontScale.ts`

- `type FontScale = "small" | "standard" | "large"`
- `FONT_SCALES`（顺序即界面顺序）
- `parseFontScale(value: unknown): FontScale | null` —— 未知值返回 `null`，
  由调用方决定回落（与 `parseTheme` 的"坏值也认一个"不同：这里必须能区分
  「没配」与「配成标准」，否则 iOS 缺键时会误判成桌面默认值）。
- `defaultFontScale(isMobile: boolean): FontScale` —— `small` / `standard`。
- `readCachedFontScale()` / `applyFontScale(scale)` —— 与 `theme.ts` 同构：
  localStorage 只做首屏缓存（键 `app.fontScale`，每台设备自己的存储，本身就按平台隔离），
  权威值在 `preferences.json`；`applyFontScale` 写 `<html data-font-scale>`。

### 5.2 `useSettingsState`

新增 `fontScale` 与 `changeFontScale`，逐字复用主题那套：

- 初值：`readCachedFontScale() ?? defaultFontScale(isMobile)`。
- `changeFontScale`：**乐观更新** → 写 `ui/settings/write`（只带本平台的键）→
  成功记作确认基线；失败回退到 `confirmedFontScaleRef` 基线、`setCurrentError`
  一条红字并 `force` 重渲染。
- `fontScaleTouchedRef`：用户触碰后，在途的首屏 `read` 不得用旧值覆盖（与
  `themeTouchedRef` 同一理由）。
- effect：`fontScale` 变化时 `applyFontScale`。

hook 需要新增入参 `isMobile`（App 里 `useIsMobile()` 已在此之前求值，
`useSettingsState` 的调用点在其后，无 TDZ 问题）。

### 5.3 App

- 首屏 `ui/settings/read`：按平台取 `font_scale_ios` / `font_scale_desktop`，
  `parseFontScale` 为 `null` 时保持平台默认（不要用 `null` 覆盖用户已缓存的值）。
- 传给 `SettingsGeneralTab` 的 `fontScale` / `onFontScaleChange`。
- **iOS 不再传值守**：`watchmanEnabled` / `onToggleWatchman` 传 `undefined`。
  按 `saveRelayUrl={isRemoteClient() ? undefined : ...}` 的既有先例，
  `SettingsGeneralTab` 已有的 `showWatchman` 判据（两个 props 都在才渲染）直接生效，
  组件不需要新增平台判断。

### 5.4 设置界面

「通用」页在「主题」下方加「字体大小」一栏：三个按钮（小 / 标准 / 大），
沿用主题那组 `inline-flex rounded-md border` 的观感，`aria-pressed` 标当前档。
下方补一句说明：该档位只影响这台设备的显示。

手机端已有的 CSS（分类列表 → 子页、44px 触控目标）无需改动：新增的按钮组就是
普通行内控件，`min-height` 由按钮自身内边距决定。

---

## 6. 失败处理

| 场景 | 行为 |
|---|---|
| `preferences.json` 缺文件 / 坏 JSON | 两个键都读作 `null`，客户端用平台默认；不阻断启动 |
| 键存在但值非法 | 同上，回 `null`（后端**不**做"宽松纠正"，避免把错误值当合法档位回显） |
| `ui/settings/write` 收到非法值 | `invalid_params` 失败，不落盘 |
| 写盘 IO 失败 | `internal` 失败；前端回退到确认基线并报错（与主题一致） |
| 首屏 read 在途、用户已改档 | 触碰标记生效，旧值不覆盖 |

---

## 7. 测试

**前端**

- `lib/fontScale.test.ts`：`parseFontScale` 合法/非法/大小写与空白；`defaultFontScale`
  两平台默认值；`applyFontScale` 写 `dataset` 与缓存。
- `SettingsGeneralTab.test.tsx`：三档按钮渲染、点选回调、当前档 `aria-pressed`；
  字体栏在两端都渲染；值守在 props 缺失时不渲染（复用既有用例）。
- `SettingsDialog.test.tsx`：字号 props 透传到「通用」页；iOS 下不显示值守。
- `mobileLayout.test.ts` 或新增 `fontScaleCss.test.ts`：读真实 `index.css`，断言
  四个 `--text-*` 都乘 `--font-scale`、两个档位的选择器存在、`.prose` 接了
  `--text-base`，且 16px 输入框兜底仍在。
- `useSettingsState` 的写失败回退（沿用主题同款用例的形态）。

**后端**

- `settings_store`：两键读写往返、非法值读作 `null`、写保留无关键。
- `ui/settings/read` 返回两键（缺省 `null`）；`ui/settings/write` 接受合法值、
  拒绝非法值、两键互不覆盖。

**整体**

- `cd desktop && npx tsc --noEmit && npx vitest run && npm run build`
- `cd yi-agent-rs && cargo fmt --all && just ci`

---

## 8. 验收判据

1. iPhone 上首次打开（无任何偏好）：正文 15px、`<html data-font-scale="small">`；
   桌面首次打开：16px、`<html data-font-scale="standard">`，观感与改动前一致。
2. 手机上选「大」→ 桌面打开仍是桌面自己那一档，磁盘上
   `font_scale_ios` 与 `font_scale_desktop` 各自独立。
3. iPhone 的「通用」页**没有**「后台值守」；桌面有。
4. 设置面板里的输入框仍为 16px（iOS 聚焦不放大）。
5. 聊天区 markdown 正文随档位变化（`.prose` 已接上 `--text-base`）。
