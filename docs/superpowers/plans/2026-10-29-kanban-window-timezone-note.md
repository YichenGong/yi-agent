# 看板时段窗口时区说明 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在桌面「设置 → 插件 → superpowers-kanban」面板的时段并发窗口区域，明确写出这些时刻按插件所在机器的本地时间判定，消除跨时区时的困惑。

**Architecture:** 纯前端文案改动，一处 UI 加一行说明，一处文档补同口径句子。判定逻辑（`calendar.rs` 的 `matches`/`limit_at`）、TOML 格式、`plugin/settings/read` 载荷全部不动，运行时行为零变化。

**Tech Stack:** React + TypeScript（`desktop/`），Vitest + @testing-library/react；Markdown 文档（`plugins/superpowers-kanban/INSTALL.md`）。

## Global Constraints

- 不改判定语义：`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs` 一行不动。
- 不改落盘格式：`superpowers-kanban.toml` 的 `[[window]]` 字段集合不变（`days`/`start`/`end`/`all_day`/`max_tasks`）。
- 不改协议：`plugin/settings/read` 返回不加任何字段，`KanbanSettings` 形状（`desktop/src/lib/pluginSettings.ts`）不变。
- 说明文案**无条件显示**，不做跨时区判断（前端拿不到宿主时区）。
- 不新增 CSS 类、不新增依赖；样式复用文件内已有的 `text-xs text-fg-subtle`。
- 不补 `all_day` 开关（独立既有缺口，明确不在本次范围）。
- 提交信息用 conventional commits，不写 `Co-Authored-By` 行。

---

### Task 1: 面板加时区口径说明

**Files:**
- Modify: `desktop/src/components/SuperpowersKanbanPluginSettings.tsx:155`（`<h3>` 与窗口列表 `<div>` 之间）
- Test: `desktop/src/components/SuperpowersKanbanPluginSettings.test.tsx`

**Interfaces:**
- Consumes: 无（不读 `settings`、不读 `rpc`）。
- Produces: 一段带 `data-testid="window-timezone-note"` 的 `<p>`，供 Task 1 的测试断言；后续任务不依赖它。

- [ ] **Step 1: 先写失败的测试**

在 `desktop/src/components/SuperpowersKanbanPluginSettings.test.tsx` 末尾的 `describe("SuperpowersKanbanPluginSettings", ...)` 块内、最后的 `});` 之前追加：

```tsx
  it("states which clock the window hours follow", async () => {
    // 根因回归：窗口在插件进程里按 chrono::Local 判定，UI 却只写 09:00，
    // 跨时区（桌面连远端宿主）时用户无从判断这是哪个 09:00。
    render(<SuperpowersKanbanPluginSettings rpc={scriptedRpc([{ settings: EMPTY_SETTINGS }])} />);

    const note = await screen.findByTestId("window-timezone-note");
    expect(note.textContent ?? "").toContain("运行插件的机器");
    expect(note.textContent ?? "").toContain("本地时间");
  });
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd desktop && npx vitest run src/components/SuperpowersKanbanPluginSettings.test.tsx -t "states which clock"
```

Expected: FAIL，报 `Unable to find an element by: [data-testid="window-timezone-note"]`。

- [ ] **Step 3: 加说明文案**

在 `desktop/src/components/SuperpowersKanbanPluginSettings.tsx` 中，把

```tsx
      <h3 className="mt-4 text-sm font-medium text-fg">全机时段并发窗口</h3>
```

改为

```tsx
      <h3 className="mt-4 text-sm font-medium text-fg">全机时段并发窗口</h3>
      {/*
       * 窗口的判定发生在插件进程里（`chrono::Local`），不是本设备的时区。
       * 桌面可经 relay 连远端宿主，那时 UI 在本机、墙钟在远端，用户填的
       * 09:00 与实际生效时间不同却毫无线索。前端拿不到宿主时区，判不出
       * 是否真的跨时区，所以这里无条件陈述口径，不做条件渲染。
       */}
      <p data-testid="window-timezone-note" className="mt-1 text-xs text-fg-subtle">
        以下时刻按运行插件的机器（本地时间）判定，不是本设备的时区；跨时区时两者会不一致。
      </p>
```

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd desktop && npx vitest run src/components/SuperpowersKanbanPluginSettings.test.tsx
```

Expected: PASS，该文件全部用例通过（含新加的 `states which clock the window hours follow`）。

- [ ] **Step 5: 类型检查**

```bash
cd desktop && npx tsc --noEmit
```

Expected: 无输出、exit 0。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/SuperpowersKanbanPluginSettings.tsx \
        desktop/src/components/SuperpowersKanbanPluginSettings.test.tsx
git commit -m "feat(desktop): say which clock the kanban window hours follow"
```

---

### Task 2: INSTALL.md 补同口径说明

**Files:**
- Modify: `plugins/superpowers-kanban/INSTALL.md:283-292`（「6.5 并发配额（整机一份）」里的 `[[window]]` 代码块之后）

**Interfaces:**
- Consumes: Task 1 确定的中文口径措辞（保持两处说法一致）。
- Produces: 文档中一句说明，无代码依赖。

- [ ] **Step 1: 定位插入点**

```bash
grep -n "max_tasks = 3" plugins/superpowers-kanban/INSTALL.md
```

Expected: 命中 6.5 节 `[[window]]` 示例里的那一行（示例块紧跟的段落是「判定口径是「全机峰值 − **本进程**当前运行中的会话数」：」）。

- [ ] **Step 2: 在示例块之后、判定口径段之前插入说明**

在 `max_tasks = 3` 所在代码块结束的 ``` 之后、`判定口径是「全机峰值 − ...` 那段之前插入：

```markdown
`days` / `start` / `end` 一律按**运行插件的机器**的本地时间判定（插件进程的
`chrono::Local`），不是看板界面所在设备的时区。桌面经 relay 连远端宿主时两者
可能不同，跨时区请按宿主机器的时间填写。
```

- [ ] **Step 3: 目视核对渲染**

```bash
sed -n '280,300p' plugins/superpowers-kanban/INSTALL.md
```

Expected: 新增段落位于 `[[window]]` 代码块的 ``` 之后、`判定口径是` 之前，缩进与相邻段一致（顶格）。

- [ ] **Step 4: 提交**

```bash
git add plugins/superpowers-kanban/INSTALL.md
git commit -m "docs(kanban): note that window hours follow the plugin host clock"
```

---

### Task 3: 更新项目进度文档

**Files:**
- Modify: `docs/project-management/superpowers-kanban.md`（在既有条目列表末尾追加一条）

**Interfaces:**
- Consumes: Task 1 / Task 2 的落地位置（用于写可验证判据）。
- Produces: 一条带可执行判据的 `[x]` 条目。

- [ ] **Step 1: 追加条目**

在 `docs/project-management/superpowers-kanban.md` 的条目列表末尾（与相邻条目同样格式、同样以 `—` 收尾给出验证命令）追加：

```markdown
- [x] 设置面板的时段并发窗口写明时区口径 — 窗口在插件进程内按 `chrono::Local` 判定，UI 原先只给 `09:00` 而不说是哪个时区的 `09:00`，桌面经 relay 连远端宿主时用户无从判断。修复：面板在「全机时段并发窗口」标题下加一行无条件说明（`desktop/src/components/SuperpowersKanbanPluginSettings.tsx`，`data-testid="window-timezone-note"`），`INSTALL.md` 6.5 节补同口径句子；判定逻辑、`superpowers-kanban.toml` 格式与 `plugin/settings/read` 载荷均未改动。验证：`cd desktop && npx vitest run src/components/SuperpowersKanbanPluginSettings.test.tsx` — [设计](../superpowers/specs/2026-10-29-kanban-window-timezone-note-design.md)
```

- [ ] **Step 2: 同步索引表计数**

```bash
grep -n "superpowers-kanban" docs/project-management/README.md
```

Expected: `| superpowers-kanban | 10 / 10 | [详情](./superpowers-kanban.md) |`

现状核对：模块文件 `docs/project-management/superpowers-kanban.md` 的 `## Features`
下**实际已有 11 条 `[x]`**（无 `[ ]`），索引表却写着 `10 / 10`——索引在本次改动
之前就已经落后一条。本次新增 1 条后目标值是 **12 / 12**，即把该行改为：

```markdown
| superpowers-kanban | 12 / 12 | [详情](./superpowers-kanban.md) |
```

口径说明：总数 = 已完成 + 未完成 = 12 + 0。这一行同时修掉了既有的 off-by-one。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/superpowers-kanban.md
git commit -m "docs: record the kanban window timezone note"
```

---

## Self-Review

**Spec coverage:**
- 「设置面板加说明」→ Task 1。
- 「文档补同口径」→ Task 2。
- 「不改判定/格式/协议」→ Global Constraints 钉死，且 Task 1/2 的 Files 只碰 UI 与 Markdown。
- 「测试：面板渲染后说明文案存在」→ Task 1 Step 1 与 Step 4。
- 「验证：vitest + tsc」→ Task 1 Step 4 / Step 5。
- 项目进度维护规则（CLAUDE.md 要求同 commit 更新 `docs/project-management/`）→ Task 3。

**Placeholder scan:** 无 TBD/TODO；每个代码步骤都给了可直接粘贴的内容与真实字符串（`全机并发上限`、`全机时段并发窗口`，已核对组件源码）。

**Type consistency:** 本次不新增任何类型或函数；`data-testid="window-timezone-note"` 在 Task 1 的 UI 与测试中同名一致；`KanbanSettings` 不被触碰。
