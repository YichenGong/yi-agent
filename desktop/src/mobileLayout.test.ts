/** @vitest-environment node */
import { describe, it, expect } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

/**
 * 手机端「贴合画面」的静态门禁。
 *
 * 与 `MarkdownText.test.tsx` 的浅色主题用例同源：直接读**真实**的 `index.html`
 * 与 `index.css`，断言的是真正会打包进去的产物，而不是测试里另写一份常量。
 *
 * 三条都是 iOS 上「上下高度不对」的直接成因，任何一条回退都会让手机端再次
 * 出现刘海遮挡、底部输入框被顶出画面或整页可上下滑动。桌面端不匹配 `[data-mobile]`
 * 前缀，因此这些选择器对桌面逐字节无影响。
 */
const dir = String((import.meta as unknown as { dirname: string }).dirname);
const html = () => readFileSync(`${dir}/../index.html`, "utf8");
const css = () => readFileSync(`${dir}/index.css`, "utf8");

describe("phone viewport metadata", () => {
  it("opts into the notch/rounded-corner safe area", () => {
    // 没有 viewport-fit=cover，`env(safe-area-inset-*)` 恒为 0，下面所有安全区留白
    // 全部失效。
    expect(html()).toMatch(/viewport-fit=cover/);
  });
});

describe("phone layout CSS", () => {
  it("fits the shell to the dynamic viewport, with a vh fallback", () => {
    // `100vh` 在 iOS 上大于可视高度（Safari 工具栏 + 刘海区都不计入），底部输入框
    // 因此被顶出画面。`100dvh` 跟的是动态可视高度；但 Safari < 15.4 不认识 `dvh`，
    // 必须先给 `100vh` 回退，否则会落到 `height: auto`（比不修更糟）。
    const rule = /\[data-mobile="true"\]\s+body\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(rule).toMatch(/height:\s*100vh/);
    expect(rule).toMatch(/height:\s*100dvh/);
    // 覆盖必须晚于回退（后者写在前、前者写在后）。
    expect(rule.indexOf("100vh")).toBeLessThan(rule.indexOf("100dvh"));
  });

  it("boxes the safe areas into that height so they are not added on top", () => {
    // body 的上下安全区内边距必须与动态视口高度同处一个 `border-box` 高度的盒子里，
    // 否则两条留白会把内容整体撑高，抵消高度修正。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+body\s*\{[^}]*padding-top:\s*env\(safe-area-inset-top[^}]*padding-bottom:\s*env\(safe-area-inset-bottom/,
    );
  });

  it("hides the desktop-only titlebar strip on the phone", () => {
    // iOS 上没有红绿灯按钮，那条 32px 的空条只白占高度。
    expect(css()).toMatch(/\[data-mobile="true"\]\s+\.app-titlebar\s*\{[^}]*display:\s*none/);
  });

  it("floats the drawer toggle over the main column instead of a rail", () => {
    // 收起抽屉时聊天列本该拿满整宽；那颗按钮原先坐在一条常驻的 44px 竖条列里，白占
    // 横向宽度。改为浮层后，让位交给主列自己的顶部内边距：按钮 top(4px) + 高 44px，
    // 内容从 48px 起，首条消息与看板顶栏都不会被压住。
    expect(css()).toMatch(/\[data-mobile="true"\]\s+\.app-main\s*\{[^}]*padding-top:\s*48px/);
    // 竖条按钮那条补白规则必须消失：`body` 已经加了同一段安全区，两处叠加会把图标
    // 下推两倍——按钮已不再是那条竖条。
    expect(css()).not.toMatch(/\.app-mobile-toggle\s*\{[^}]*padding-top/);
  });

  it("keeps the status bar on a single line on the phone", () => {
    // 状态栏是一条单行 flex（连接点 / 状态 / cwd / 用量）。手机上 cwd 与用量都是长
    // 文本，一旦被压到内容下限以下，「N in / M out」会折成多行，整条底栏随之变高
    // （实测单行 33px、折行后 81px）——正是用户报的「底栏过高」。
    //
    // 修法：让状态栏文本不可折行。挤压因此只能落到会截断的 cwd 上（它是唯一的
    // `.truncate`），用量按钮保持自身宽度、恒为一行。桌面不匹配 `[data-mobile]`，
    // 逐字节不变；手机端这条也只在文本真的需要折行时才与桌面不同。
    const rule =
      /\[data-mobile="true"\]\s+\.app-statusbar\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(rule).toMatch(/white-space:\s*nowrap/);
  });

  it("tints the safe-area bands so they read as panel, not as a void", () => {
    // 上下两条安全区留白必须由 `body` 自己铺色。不铺色时它们是**没有背景**的区域，
    // 深色主题下就是两条很长的黑边——正是用户报的「上下黑边太长」。`--panel` 与
    // 底部 `MessageInput` / `StatusBar` 同色，留白与面板接成一片，看不出边界。
    expect(css()).toMatch(/\[data-mobile="true"\]\s+body\s*\{[^}]*background:\s*var\(--panel\)/);
  });

  it("trims the bottom inset to the clearance the home indicator needs", () => {
    // 整段 `safe-area-inset-bottom`（iPhone 14 Pro 上 34pt）全留，输入区被顶得过高，
    // 底边因此显得空。收窄到一个固定的 12pt 净空后，Home Indicator 仍有净空而不浪费
    // 高度；`max(..., 0)` 让没有底部安全区的设备回到 0。
    const src = css();
    expect(src).toMatch(/@media\s*\(max-width:\s*767\.98px\)/);
    expect(src).toMatch(
      /padding-bottom:\s*max\(env\(safe-area-inset-bottom,\s*0px\)\s*-\s*12px,\s*0px\)/,
    );
  });

  it("gives the drawer's scrolling element its own safe-area insets", () => {
    // 抽屉是 `position: fixed`，脱离文档流，父级的内边距管不到它。安全区要落在
    // 真正会滚动的 `aside` 上，滚动到底时最后一行才不被 Home Indicator 压住。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+\.app-sidebar\s+aside\s*\{[^}]*safe-area-inset-top[^}]*safe-area-inset-bottom/,
    );
  });

  it("applies the drawer safe area exactly once, on the scrolling element", () => {
    // 包裹层 `.app-sidebar` 与内层 `aside` 都补 `safe-area-inset-top` 时，两条内边距
    // 首尾相接叠成两倍：fixed 包裹层从 `top: 0` 起，内层 `aside` 又落进包裹层的内容盒
    // （padding 之内），iPhone 14 Pro 上 59pt 变 118pt，「+ New thread」与整个文件夹
    // 列表被整体下推，顶出一条点不到的大空白。安全区只允许出现在 `aside` 上。
    const wrapper =
      /\[data-mobile="true"\]\s+\.app-sidebar\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(wrapper).not.toMatch(/safe-area-inset-top/);
    expect(wrapper).not.toMatch(/safe-area-inset-bottom/);
  });

  it("keeps the search box at 16px so iOS cannot auto-zoom on focus", () => {
    // WKWebView 对字号 < 16px 的输入框会在聚焦时自动放大页面，而 `lockZoom`
    // 只能禁掉用户的手动捏合缩放，挡不住这条自动放大——放大后页面不再贴合屏幕，
    // 每次滑动变成两轴平移，正是「缩放之后就需要拖拽」的同一个症状。
    const rule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+input\[type="search"\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/font-size:\s*16px/);
  });

  it("hugs the drawer so its blank strip cannot swallow the close-tap", () => {
    // 「抽屉打开后有一半空白点不缩回」的根因：包裹层 `.app-sidebar` 是 fixed 的
    // 空盒子，比内层 `aside` 宽出一截（256px vs 82vw≈320px @390pt）。那一截透明
    // 空白仍接收点击，而背板（z-20）在它之下，于是落在该带里的点击到不了背板。
    // 两道保险都必须钉住：包裹层贴合抽屉宽度，且透明区域不接收指针事件；抽屉本体
    // 再把指针事件收回来，滚动/长按/行内点击照常。
    const wrapper =
      /\[data-mobile="true"\]\s+\.app-sidebar\s*\{([^}]*overscroll-behavior[^}]*)\}/.exec(css())?.[1] ??
      "";
    expect(wrapper).toMatch(/width:\s*auto/);
    expect(wrapper).not.toMatch(/min\(\s*82vw/);
    expect(wrapper).toMatch(/pointer-events:\s*none/);
    const asideRule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+aside\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(asideRule).toMatch(/pointer-events:\s*auto/);
  });

  it("constrains free-standing images so overflow-x-hidden cannot clip them", () => {
    // typography 基准里 `img` 只有上下外边距、没有 `max-width`；会话列一旦
    // `overflow-x-hidden`，超宽图片被裁掉的部分将永久不可达。
    const src = readFileSync(`${dir}/components/MarkdownText.tsx`, "utf8");
    expect(src).toContain("prose-img:max-w-full");
  });

  it("refuses pinch-zoom, without also refusing scrolling", () => {
    // WKWebView 默认允许双指缩放；放大后页面不再贴合屏幕，之后每次滑动都变成
    // 两轴平移（用户报的「缩放之后就需要拖拽」）。`pan-x pan-y` 只禁缩放：
    // 抽屉列表与 `overflow-x-auto` 的表格仍可滑动。
    expect(css()).toMatch(/html\[data-mobile="true"\]\s*\{[^}]*touch-action:\s*pan-x pan-y/);
  });

  it("suppresses the native long-press callout on sidebar thread rows", () => {
    // 按住会话行时 iOS 会先弹文字选择/放大镜 callout，把长按抢走——长按=右键就
    // 永远触发不了。这三条都必须限定在 [data-mobile="true"] 下，桌面端逐字节不变。
    const rule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+\[data-thread-row\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/-webkit-touch-callout:\s*none/);
    expect(rule).toMatch(/-webkit-user-select:\s*none/);
    expect(rule).toMatch(/user-select:\s*none/);
    // 组头也是长按目标，同样必须关掉原生 callout，否则组头菜单在 iOS 上按不出来。
    const headerRule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+\[data-ws-header\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(headerRule).toMatch(/-webkit-touch-callout:\s*none/);
    expect(headerRule).toMatch(/-webkit-user-select:\s*none/);
    expect(headerRule).toMatch(/user-select:\s*none/);
  });

  it("keeps the rename field's text selectable on the phone", () => {
    // 关掉 user-select 后，重命名输入框里选不中字——那是同一段 CSS 的必须豁免。
    const rule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+\[data-thread-row\]\s+input\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/-webkit-user-select:\s*text/);
    expect(rule).toMatch(/user-select:\s*text/);
  });

  it("turns the phone settings into a full-screen page with a top tab bar", () => {
    // 「设置区域过于小」的根因：弹窗 `w-[70vw]` 在 390pt 上只有 273px，左侧竖栏又是
    // 写死的 `w-40`(160px)，占去 59%，内容面板只剩 111px 且横向溢出滚不到。手机端
    // 必须整屏（`position: fixed` + `inset: 0`）并让导航横排，内容才拿得到全部宽度。
    const shell =
      /\[data-mobile="true"\]\s+\.settings-shell\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(shell).toMatch(/position:\s*fixed/);
    expect(shell).toMatch(/inset:\s*0/);
    expect(shell).toMatch(/flex-direction:\s*column/);
    // 弹窗那套尺寸必须被解除，否则 fixed 了却仍是 70vw/70vh 的小盒子。
    expect(shell).toMatch(/width:\s*auto/);
    expect(shell).toMatch(/max-width:\s*none/);
    expect(shell).toMatch(/height:\s*auto/);

    const nav = /\[data-mobile="true"\]\s+\.settings-nav\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(nav).toMatch(/flex-direction:\s*row/);
    // 竖栏那条右边框要撤掉，改由底边框分栏。
    expect(nav).toMatch(/border-right:\s*0/);

    const tab =
      /\[data-mobile="true"\]\s+\.settings-nav\s*>\s*\[role="tab"\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    // 每格等宽：320pt 上也有 80px，四字标签不折行。
    expect(tab).toMatch(/flex:\s*1 1 0/);
    expect(tab).toMatch(/white-space:\s*nowrap/);
    // 44px 触控下限（Apple HIG）：上下各 12px 内边距 + 行高。
    expect(tab).toMatch(/padding-top:\s*12px/);
    expect(tab).toMatch(/padding-bottom:\s*12px/);
  });

  it("hides the settings backdrop on the phone", () => {
    // 全屏页没有浮层，遮罩只剩白占屏；关闭改走右上角按钮或 Escape。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+\[data-settings-backdrop\]\s*\{[^}]*display:\s*none/,
    );
  });

  it("gives the phone settings page its own safe-area insets", () => {
    // 全屏页是 `fixed`，脱离 `body` 的内边距，顶部（刘海）与底部（Home Indicator）
    // 都必须自己补：顶部落在头部栏，底部落在唯一的滚动容器上，且各只补一次。
    const head =
      /\[data-mobile="true"\]\s+\.settings-header\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(head).toMatch(/padding-top:\s*max\(env\(safe-area-inset-top/);

    const panel =
      /\[data-mobile="true"\]\s+\.settings-panel\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(panel).toMatch(/padding-bottom:\s*env\(safe-area-inset-bottom/);

    // shell 自身不得再补底部安全区，否则与面板那条叠成两倍。
    const shell =
      /\[data-mobile="true"\]\s+\.settings-shell\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(shell).toMatch(/padding-bottom:\s*0/);
  });

  it("puts the settings header above the tab bar on the phone", () => {
    // DOM 顺序是 [nav, (header, panel)]，而手机上要的视觉顺序是 [header, nav, panel]。
    // 标题栏若排在 Tab 条之后：(1) 观感上「设置」标题跑到 Tab 条下面；(2) 更实际的，
    // 顶部安全区（刘海）内边距挂在 `.settings-header` 上，那段留白会落到屏幕中部，
    // Tab 条反而紧贴状态栏被压住。跨嵌套层级要靠 `display: contents` 让包裹层「消失」
    // 再对子元素用 `order`——真机 Chrome 实测修正前 header.y=0 但渲染在 nav 之后。
    const bodyRule =
      /\[data-mobile="true"\]\s+\.settings-body\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(bodyRule).toMatch(/display:\s*contents/);

    // 同一选择器会有多条规则块（安全区、order…），必须汇总全部声明再断言；
    // 只取第一条会漏掉 `order`。
    const blocks = (selector: string) =>
      [...css().matchAll(new RegExp(`\\[data-mobile="true"\\]\\s+\\${selector}\\s*\\{([^}]*)\\}`, "g"))]
        .map((m) => m[1])
        .join("\n");

    const head = blocks(".settings-header");
    const nav = blocks(".settings-nav");
    const panel = blocks(".settings-panel");

    const orderOf = (rule: string) => Number(/order:\s*(\d+)/.exec(rule)?.[1] ?? "0");
    expect(orderOf(head)).toBe(1);
    expect(orderOf(nav)).toBe(2);
    expect(orderOf(panel)).toBe(3);
    // 升为 flex item 后必须锁住高度，否则会被纵向 flex 压缩。
    expect(head).toMatch(/flex-shrink:\s*0/);
  });

  it("keeps the settings inputs at 16px so iOS cannot auto-zoom on focus", () => {
    // 面板里的文本输入框是 14px（`text-sm`），低于 iOS 自动放大阈值 16px。`lockZoom`
    // 只禁手动捏合，挡不住这条自动放大——放大后页面不再贴合屏幕，每次滑动变成两轴
    // 平移。与侧栏搜索框那条同源、同理由。
    const rule =
      /\[data-mobile="true"\]\s+\.settings-panel\s+(?:input|select)[^{]*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/font-size:\s*16px/);
  });
});
