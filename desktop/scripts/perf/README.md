# 长会话性能：参考基线复现脚本

本目录是 `docs/superpowers/specs/2026-10-18-long-session-perf-design.md`
§7 所述的**只读**复现脚本。它们不修改仓库任何文件，只用来复算该 spec §1.1
的实测曲线（"改前"基线）。

## 为什么放在这里

spec 的验收门禁是**结构性不变量**（见 spec §6），而这些脚本产出的是**绝对耗时
数字**——数字不进 CI（机器快慢会飘），但必须可复算，否则"参考基线"是一句空话。

## 前置

脚本直接消费仓库里的**真实生产代码**，不复制副本：

- `session.ts` 经 esbuild 打包后由 node 直接跑（数据层）。
- `react` / `react-dom` / `react-markdown` / `jsdom` 取自 `desktop/node_modules`。

因此必须先装好 `desktop` 的依赖：`cd desktop && npm install`。

## 用法

```bash
cd desktop
npm install                 # 只需一次
node scripts/perf/run.mjs   # 跑全部四项，打印 spec §1.1 的曲线
```

单项也可以直接跑：

```bash
node scripts/perf/session.mjs     # §1.1(a)：每个 delta 的 findIndex 开销
node scripts/perf/index-lookup.mjs # §3.1：三种查找策略的对照（关键证据）
node scripts/perf/render.mjs      # §1.1(b)：每 delta 的全量重渲染开销
node scripts/perf/markdown.mjs    # §1.1(c)：流式 markdown 的平方级退化
node scripts/perf/wire.mjs        # §1.1(a) 的帧字节放大（第五项的背景数据）
node scripts/perf/checkpoint.mjs  # §3.4：checkpoint 为何不能减载（"不丢东西"）
node scripts/perf/turn.mjs        # §2.5：一个回合值多少（含历史深度扫描）
node scripts/perf/mutation-trap.mjs # §3.2：memo 比较器读 item.text 会冻结流式
```

`checkpoint.mjs` 读的是**这台机器上真实的 `.partial.json`**（沿 cwd 向上找
`.yi-agent/threads`，再试 `$HOME`）；找不到任何快照时它**如实报告"没有可测的
数据"并退出 0**，而不是构造假数据冒充基准。

`run.mjs` 会把 `session.ts` 打包到系统临时目录（`os.tmpdir()`）下的
`yi-agent-perf/`，不写入仓库。

## 诚实边界

- 前端数字跑在 **node + jsdom** 下，不是 WebKit。**绝对数值不可等同于真机**；
  可信的是**增长趋势**（O(N) / O(N²)），它与实现一一对应。
- `checkpoint.mjs` 读的是**这台机器上真实的 `.partial.json`**，因此它的
  "字节构成"随你本地的会话而变。它找不到快照时**如实报告并退出 0**，
  不会构造假数据。
- TUI（§1.1(e)、§3.5）不在这里复现——它由 Rust 侧的
  `lines_call_count` / `rebuild_count` 计数器测试覆盖（见 spec §6），
  那是更合适的载体。