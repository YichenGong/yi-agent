# 会话显示收敛（D01–D09）验收记录

对应设计：[会话内容显示不全修复 spec](../superpowers/specs/2026-11-08-session-completeness-repair-design.md)
§4.4 / §8；实施计划：[显示收敛 A1–A5](../superpowers/plans/2026-11-08-session-display-convergence-impl.md)。

用户报的症状是：**会话内容显示不全，切出去再切回来才看到更多**。根因不是单点
bug，而是「视口测量 → 窗口决策 → 位置写入」这条链上多处各自失效，下面按机制分组。

## 运行方式

```sh
cd desktop
npm run test:browser:session-completeness     # D 组 9 条（真实 Chrome）
npx vitest run                                # 单测全量
npx tsc --noEmit
npm run build
```

要求 Node 22+ 与 Chrome。环境变量见 `desktop/scripts/browser/README.md`。

## 逐行结果（Chrome 真实布局）

| ID | 结果 | 证据要点 |
| --- | --- | --- |
| D01 | PASS | A→B→A 后一次 `scrollTop=60802` 即覆盖视口；`mounted=400`（含恒挂 tail 的名额） |
| D02 | PASS | 3000 条滚到历史顶部，视口有可视行，`mounted ≤ 400` |
| D03 | PASS | `focus(100)` 窗口含 100；向下拖后窗口跟到尾部 `[2600,3000)`、`following=true`、5 次采样几何完全不变 |
| D04 | PASS | 等长 A/B 不继承位置；B 缩短到 592 后 `windowEnd ≤ 592`；宽度 560/900 双向都覆盖且 ≤ 400 |
| D05 | PASS | 读者锚点行在下方增高 40 行后漂移 0px |
| D06 | PASS | tail 与非 tail 增长都保持贴底；释放后上方增高，锚点漂移 ≤ 2px |
| D07 | PASS | 晚到目标 `focus(0)`→`focus(300)`：目标挂载并对位，`displayError=null` |
| D08 | PASS | 历史远离 tail + 增高/折叠/改宽度：`mounted ≤ 400`、无横向溢出 |
| D09 | PASS | 静止 500ms `scrollTop` 不变 |

2026-11-08 实测：`9/9 display checks passed`。

## 判别性（每条都在回退后变红，恢复后变绿）

不做这一层就无法区分「真的修好了」与「本来就碰巧通过」。

| 变异 | 变红的断言 |
| --- | --- |
| `nextWindow` 不接收用户贴底意图 | D03 `viewport went blank after reversing direction (top=611204 mounted=362 win=[0,361))` |
| 宿主 `requestFocus` 后不做对位 | D08 `history away from the tail must be visible` |
| `estimateFocusIndex` 的 gap 符号取反 | 单测：`expected +0 to be 511`、`expected 961 to be 499`、`expected 61 to be 30` |
| 高度账本 memo 退回 `[]`（不按会话重建） | 单测：`切会话必须换一本高度账本` → `expected 39200 to be 8400` |
| `heightBook.record` 去掉 `<= 0` 守卫 | 单测 5 条红（含 `spacer 跟着收敛（0 高度必须被忽略）`） |
| 记账退回「只在窗口变化时」 | 单测 2 条红 |
| `anchorIds` 分支禁用 | 单测：`视口落在未挂载区间` → `expected 2940 to be less than or equal to 600` |
| `heightRevision` 只在窗口变化时生效 | 单测：spacer 不跟随中位数 → `expected 5600 to be 70000` |
| `hidden` 时不暂停调度 | 单测：`hidden 期间不排帧` → `expected 2740 to be 2940` |
| spacer 只在渲染里更新（不写 DOM） | `stream-live`：贴底断掉（gap 280px） |

## 本次修掉的真实缺陷（不是重构）

1. **tail 名额 off-by-one。** `nextWindow` 用裁剪**前**的 `end` 判断 tail 是否在
   窗口内。原本贴到末尾的窗口被裁一刀后 `end` 退离末尾，恒挂 `LiveTail` 变回第
   401 条，DOM 实测 401 超预算。改为迭代到不动点。
2. **贴底意图没有进入窗口计算。** 用户把滚动条拖到底后，视口落在下方 spacer 里，
   而「跟随中不补页」直接判 `settled`，窗口永远停在中段——**不滚动就一片空白**。
   现在贴底意图会传给 `nextWindow`，窗口直接收回尾部。
3. **`estimateFocusIndex` 的 gap 符号反了。** 与 `measureTranscript` 的符号约定
   相反，导致「往下翻」被读成「往上翻」，每次补页都跳回第 0 条。
4. **跳转只开窗、不移动视口。** 搜索跳到未挂载历史时，即使窗口换到目标附近，
   两个 spacer 一长一短使 `scrollHeight` 几乎不变，浏览器不会纠正 `scrollTop`，
   用户仍然什么都看不到。现在开到位后显式把目标对到视口顶。
5. **记账时机与贴底写入抢帧。** 在中位数变化时 `setState` 会插入一次渲染，把刚
   写好的贴底位置甩开（实测 gap 280px）。改为每次布局提交直接写 spacer 的 DOM
   高度，只在 spacer 真的占位时才 `setState`。
6. **隐藏页面仍在排帧。** 后台标签页 rAF 被节流，切回可见时可能一次连跑多帧，
   把基于旧布局的结论应用到新布局上。现在不可见时不排帧，切回时补一次。

## 未验收（必须保持未勾选）

- macOS 原生 WKWebView / 真机：见 `desktop/scripts/browser/README.md` 末尾的人工
  表，全部保持 `[ ]`。Chrome 的排版与滚动实现和 WebKit 不同，IME、触控惯性与
  `dvh` 只在真机上成立。
- `npm run test:browser:session-view` 的 `user scroll takes control from drift
  correction`（`expected 2000, got 2240`）在 A3、A4 两个基线与当前工作区**同样
  失败**，属既有基线问题，不是本次回归；单独跟踪。
