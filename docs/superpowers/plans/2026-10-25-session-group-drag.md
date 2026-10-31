# 会话分组：拖动进出 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用拖拽替换组框头上的「+」按钮：拖会话行到组框即加入，从组框拖到框外会话行即移出，多选时整批操作。

**Architecture:** 全部改动集中在前端 `ThreadSidebar.tsx`。把「置顶区重排」与「进出分组」两套拖拽手势继续分开：`dragId` 管重排，把它替换成 `dragPayload: string[]` 管进出分组。会话行改为整行可拖，但按着 Cmd/Ctrl/Shift 时不可拖，从而保住点击多选。落点语义与批量提交保持不变，仍走 `onSetGroupMembers` 整表替换。

**Tech Stack:** React + TypeScript + Vitest + @testing-library/react（jsdom）。

## Global Constraints

- 不做手动排序：侧栏顺序恒由服务端 `updated_at` 倒序决定，拖拽不改变位置。
- 不跨 workspace：跨目录落点直接忽略，不弹错。
- 多维不改变服务端协议：仍用 `onSetGroupMembers(workspace, groupId, threadIds)` 整表替换。
- 已在目标组内的会话幂等跳过：不报错、不改顺序、不产生多余写入。
- 移出时目标组 = `dragstart` 那一行所在的**第一个**组（`groupsForThread(id)[0]`），不波及其它组。
- 移出落点只认**会话行**；目录标题与框外空白不是落点。
- 置顶区整行可拖始终成立（重排），不受修饰键影响。

## File Structure

- `desktop/src/components/ThreadSidebar.tsx` — 组件与手势逻辑（唯一实现文件）。
  - 状态层：`modifierDown`、`dragPayload`、`groupDropTarget`、`dropOverThread`。
  - 提交层：`dropIntoGroup`、`dropOutOfGroup`、`dropBetweenGroupsNoop`（不需要）。
  - 渲染层：`renderThread` 的 `draggable`/`onDragStart`/`onDragOver`/`onDrop`，组框的落点处理，项目组头的落点处理。
- `desktop/src/components/ThreadSidebar.test.tsx` — 断言文件（改写 4 例、删 2 例、新增 6 例）。
- `docs/project-management/desktop.md` — 进度登记。
- 不动：`desktop/src/lib/sessionGroups.ts`、任何 Rust 文件。

---

### Task 1: 修饰键按下时整行不可拖

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx:302`（state 区）、`:881`（`canDragIntoGroup`）、`:896`（`draggable`）
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: 无（本任务自足）。
- Produces: 组件内 `modifierDown: boolean`，以及 `dragEnabled` 语义扩展——「普通会话行在无修饰键时整行可拖」。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/components/ThreadSidebar.test.tsx` 的 `describe("拖动会话进组", ...)` 块内，把 `it("把框外会话拖到组框上即加入该组"` 之前的两个既有用例删掉（第 1697 行的「普通会话行本身不可拖」、第 1713 行的「行内拖拽手柄仍可发起进组拖拽」），替换为下面三例：

```tsx
  it("无修饰键时普通会话行整行可拖", () => {
    const { container } = renderSidebar({ groups: dragBase });
    const row = container.querySelector<HTMLElement>('[data-thread-row="c"]')!;
    expect(row.getAttribute("draggable")).toBe("true");
    expect(container.querySelector("[data-drag-handle]")).toBeNull();
  });

  it("按着 Cmd 时整行不可拖（否则原生拖拽会吞掉点击多选）", () => {
    // 整行 draggable 后，按下再移动一两像素就触发原生 dragstart，浏览器随即
    // 不再派发 click——「点了三条只记到两条」正是这条路。修饰键按下即摘掉
    // draggable，点击语义逐字不变。
    const { container } = renderSidebar({ groups: dragBase });
    fireEvent.keyDown(document, { key: "Meta" });
    const row = container.querySelector<HTMLElement>('[data-thread-row="c"]')!;
    expect(row.getAttribute("draggable")).not.toBe("true");
    fireEvent.keyUp(document, { key: "Meta" });
    expect(
      container.querySelector<HTMLElement>('[data-thread-row="c"]')!.getAttribute("draggable"),
    ).toBe("true");
  });

  it("按着 Cmd 时点击三条仍然只选三条、不触发拖拽", () => {
    const onSelect = vi.fn();
    const { container } = renderSidebar({ groups: dragBase, onSelect });
    // `dragBase` 里 a 已在 g1 内，折叠态下该行不渲染，先展开。
    fireEvent.click(screen.getByLabelText("展开分组 登录模块"));
    fireEvent.keyDown(document, { key: "Meta" });
    fireEvent.click(screen.getByText("A"), { metaKey: true, detail: 1 });
    fireEvent.click(screen.getByText("B"), { metaKey: true, detail: 2 });
    fireEvent.click(screen.getByText("C"), { metaKey: true, detail: 3 });
    expect(container.querySelector("[data-selection-bar]")?.textContent).toContain("新建组（3）");
    expect(onSelect).not.toHaveBeenCalled();
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx -t "整行可拖"
```

预期：FAIL。`[data-thread-row="c"]` 的 `draggable` 目前是 `null`（普通行不可拖），且 `[data-drag-handle]` 仍存在。

- [ ] **Step 3: 加 `modifierDown` 状态与监听**

在 `ThreadSidebar.tsx` 的 `const [addingTo, setAddingTo] = useState<string | null>(null);`（第 300 行）**之前**插入：

```tsx
  /**
   * 是否有修饰键（Cmd/Ctrl/Shift）正被按住。
   *
   * 会话行改成整行可拖之后，原生拖拽会把「按下 + 轻微移动」当成拖拽起点，随后
   * 不再派发 click——多选靠点击，一旦被吞就「点了三条只记到两条」。按下修饰键
   * 时整行摘掉 `draggable`，点击语义回到加手势之前。
   *
   * 用 `e.key` 判定而不是 `e.metaKey`：`keyup` 时 `metaKey` 已经为 false，
   * 读不到「刚才松开的是谁」。
   */
  const [modifierDown, setModifierDown] = useState(false);
```

然后在组件内任意一个既有 `useEffect` 附近（推荐紧接 `useEffect` import 已在文件顶部、找一个既有的 `useEffect(() => {` 之后）追加：

```tsx
  useEffect(() => {
    const onDown = (e: KeyboardEvent) => {
      if (e.key === "Meta" || e.key === "Control" || e.key === "Shift") setModifierDown(true);
    };
    const onUp = (e: KeyboardEvent) => {
      if (e.key === "Meta" || e.key === "Control" || e.key === "Shift") setModifierDown(false);
    };
    // 窗口失焦（Cmd+Tab 切走）后 keyup 不会到达，必须复位，否则侧栏永久不可拖。
    const onBlur = () => setModifierDown(false);
    document.addEventListener("keydown", onDown);
    document.addEventListener("keyup", onUp);
    window.addEventListener("blur", onBlur);
    return () => {
      document.removeEventListener("keydown", onDown);
      document.removeEventListener("keyup", onUp);
      window.removeEventListener("blur", onBlur);
    };
  }, []);
```

- [ ] **Step 4: 改 `canDragIntoGroup` 与 `draggable`**

把第 878-881 行的注释与 `canDragIntoGroup` 整体替换为：

```tsx
    // 整行可拖：手柄几乎没人发现，而拖拽是「把会话放进框」最直觉的表达。
    // 按着修饰键时不拖——那时用户点的是多选，不是拖拽（见 `modifierDown`）。
    // 置顶行例外：那里整行拖拽就是重排，始终成立。
    const canDragIntoGroup =
      dragEnabled && opts.rowAttr === "group" && !modifierDown && editingId !== t.thread_id;
```

第 896 行改为：

```tsx
        draggable={dragging ? editingId !== t.thread_id : canDragIntoGroup}
```

`onDragStart`（第 897 行）改为：

```tsx
        onDragStart={
          dragging
            ? () => setDragId(t.thread_id)
            : canDragIntoGroup
              ? () => setDragThread(t.thread_id)
              : undefined
        }
```

本步骤先复用既有的 `dragThread`（单条）。Task 2 会把它整体换成 `dragPayload` 与 `onGroupDragStart`。

- [ ] **Step 5: 删掉手柄**

删除第 1045-1075 行整块（`{canDragIntoGroup && ( ... ⠿ ... )}`）。

- [ ] **Step 6: 跑测试确认通过**

```bash
cd desktop && node_modules/.bin/tsc --noEmit && node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx
```

预期：PASS。此时「把框外会话拖到组框上即加入该组」用例会失败（它仍找 `data-drag-handle`），在 Task 2 改写。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): 会话行整行可拖，按修饰键时让位给点击多选"
```

---

### Task 2: 拖拽 payload 快照为「整批」

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx:510-546`（`dragThread` → `dragPayload`，两个 drop 函数改批量）
- Test: `desktop/src/components/ThreadSidebar.test.tsx:2043`（`describe("拖动会话进组")`）

**Interfaces:**
- Consumes: Task 1 的 `canDragIntoGroup`。
- Produces:
  - `onGroupDragStart(t: ThreadSummary): void` — 快照 payload 并设置 `dragPayload`。
  - `dropIntoGroup(workspace: string, group: SessionGroup): void`
  - `dropOutOfGroup(): void` — 不再需要 workspace 参数，目标组由 `dragPayload[0]` 推出。

- [ ] **Step 1: 写失败测试**

在 `describe("拖动会话进组")` 里追加：

```tsx
  it("拖动一条未选中的行到组框只加这一条", async () => {
    const onSetGroupMembers = vi.fn().mockResolvedValue(true);
    const { container } = renderSidebar({ groups: dragBase, onSetGroupMembers });
    const row = container.querySelector<HTMLElement>('[data-thread-row="c"]')!;
    const box = container.querySelector<HTMLElement>('[data-group-box="g1"]')!;
    fireEvent.dragStart(row, { dataTransfer: { setData: vi.fn(), effectAllowed: "" } });
    fireEvent.drop(box, { dataTransfer: { getData: () => "c" } });
    await waitFor(() => expect(onSetGroupMembers).toHaveBeenCalledWith("/work/projA", "g1", ["a", "c"]));
  });

  it("选中三条后拖其中一条进组，整批三条一起加，已在组内的跳过", async () => {
    const onSetGroupMembers = vi.fn().mockResolvedValue(true);
    const { container } = renderSidebar({ groups: dragBase, onSetGroupMembers });
    fireEvent.click(screen.getByLabelText("展开分组 登录模块"));
    fireEvent.click(screen.getByText("A"), { metaKey: true, detail: 1 });
    fireEvent.click(screen.getByText("B"), { metaKey: true, detail: 2 });
    fireEvent.click(screen.getByText("C"), { metaKey: true, detail: 3 });
    fireEvent.keyUp(document, { key: "Meta" });
    const row = container.querySelector<HTMLElement>('[data-thread-row="c"]')!;
    const box = container.querySelector<HTMLElement>('[data-group-box="g1"]')!;
    fireEvent.dragStart(row, { dataTransfer: { setData: vi.fn() } });
    fireEvent.drop(box, { dataTransfer: { getData: () => "c" } });
    // a 已在组里 → 跳过；b、c 追加。顺序 = 组内既有 + 新追加。
    await waitFor(() =>
      expect(onSetGroupMembers).toHaveBeenCalledWith("/work/projA", "g1", ["a", "b", "c"]),
    );
  });

  it("三条都在组内时拖其中一条到框外会话行，三条一起移出", async () => {
    const onSetGroupMembers = vi.fn().mockResolvedValue(true);
    const three: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          thread("a", "A", "/work/projA"),
          thread("b", "B", "/work/projA"),
          thread("c", "C", "/work/projA"),
          thread("d", "D", "/work/projA"),
        ],
        groups: [{ id: "g1", name: "登录模块", description: "", threadIds: ["a", "b", "c"] }],
      },
    ];
    const { container } = renderSidebar({ groups: three, onSetGroupMembers });
    fireEvent.click(screen.getByLabelText("展开分组 登录模块"));
    fireEvent.click(screen.getByText("A"), { metaKey: true, detail: 1 });
    fireEvent.click(screen.getByText("B"), { metaKey: true, detail: 2 });
    fireEvent.click(screen.getByText("C"), { metaKey: true, detail: 3 });
    fireEvent.keyUp(document, { key: "Meta" });

    const row = container.querySelector<HTMLElement>('[data-thread-row="a"]')!;
    const outside = container.querySelector<HTMLElement>('[data-thread-row="d"]')!;
    fireEvent.dragStart(row, { dataTransfer: { setData: vi.fn() } });
    fireEvent.dragOver(outside, { dataTransfer: { dropEffect: "" } });
    fireEvent.drop(outside, { dataTransfer: { getData: () => "a" } });
    await waitFor(() => expect(onSetGroupMembers).toHaveBeenCalledWith("/work/projA", "g1", []));
  });

  it("多选跨两个组时，从 A 组拖出只解散 A 组那批", async () => {
    const onSetGroupMembers = vi.fn().mockResolvedValue(true);
    const two: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          thread("a", "A", "/work/projA"),
          thread("b", "B", "/work/projA"),
          thread("c", "C", "/work/projA"),
          thread("d", "D", "/work/projA"),
          thread("e", "E", "/work/projA"),
        ],
        groups: [
          { id: "g1", name: "登录模块", description: "", threadIds: ["a", "b"] },
          { id: "g2", name: "支付模块", description: "", threadIds: ["c", "d"] },
        ],
      },
    ];
    const { container } = renderSidebar({ groups: two, onSetGroupMembers });
    fireEvent.click(screen.getByText("A"), { metaKey: true, detail: 1 });
    fireEvent.click(screen.getByText("C"), { metaKey: true, detail: 2 });
    fireEvent.keyUp(document, { key: "Meta" });

    const row = container.querySelector<HTMLElement>('[data-thread-row="a"]')!;
    const outside = container.querySelector<HTMLElement>('[data-thread-row="e"]')!;
    fireEvent.dragStart(row, { dataTransfer: { setData: vi.fn() } });
    fireEvent.drop(outside, { dataTransfer: { getData: () => "a" } });
    // 只有 g1 被写；g2 一动不动。
    await waitFor(() =>
      expect(onSetGroupMembers).toHaveBeenCalledWith("/work/projA", "g1", ["b"]),
    );
    expect(onSetGroupMembers).toHaveBeenCalledTimes(1);
  });

  it("拖到目录标题上不触发移出（目录标题不是落点）", async () => {
    const onSetGroupMembers = vi.fn().mockResolvedValue(true);
    const { container } = renderSidebar({ groups: dragBase, onSetGroupMembers });
    fireEvent.click(screen.getByLabelText("展开分组 登录模块"));
    const row = container.querySelector<HTMLElement>('[data-thread-row="a"]')!;
    const header = container.querySelector<HTMLElement>('[data-ws-header="/work/projA"]')!;
    fireEvent.dragStart(row, { dataTransfer: { setData: vi.fn() } });
    fireEvent.dragOver(header, { dataTransfer: { dropEffect: "" } });
    fireEvent.drop(header, { dataTransfer: { getData: () => "a" } });
    expect(onSetGroupMembers).not.toHaveBeenCalled();
  });
```

同时把 `it("把框外会话拖到组框上即加入该组"` 里取 `[data-drag-handle="c"]` 改成 `[data-thread-row="c"]`；把 `it("框内会员拖回框外空白处即移出该组"` 整例删除（被上面的三条批量用例和「目录标题不是落点」取代）；`it("拖到组框上时高亮该框"` 里的 `[data-drag-handle="c"]` 也改成 `[data-thread-row="c"]`。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx -t "拖动会话进组"
```

预期：FAIL。批量用例只收到单条（`["a","c"]` 而非 `["a","b","c"]`），跨组用例收到两次调用。

- [ ] **Step 3: 把 `dragThread` 换成 `dragPayload`**

替换第 504-546 行（从 `/**\n   * 正在被拖动的那条会话（进/出组用）。` 到 `dropOutOfGroup` 函数结束）为：

```tsx
  /**
   * 正在被拖动的会话（进/出组用），一批。
   *
   * 与 `dragId`（置顶区重排）分开：那两个手势的落点语义完全不同——重排改的是
   * 顺序，进组改的是成员表。共用一个状态会让「拖到组框上」被误判成一次重排。
   *
   * 存数组而不是单条：多选时拖任意一条 = 整批操作。payload 在 `dragstart`
   * 时快照一次，拖拽途中不再变（否则拖过界时选择集被改，落点会跟着漂）。
   */
  const [dragPayload, setDragPayload] = useState<string[] | null>(null);
  const [groupDropTarget, setGroupDropTarget] = useState<string | null>(null);
  /** 框外会话行作为「移出」落点时的悬停 id（画插入线）。 */
  const [dropOverThread, setDropOverThread] = useState<string | null>(null);

  /** 拖拽起手：在选择集里就带上整批，否则只带这一条。 */
  const onGroupDragStart = (t: ThreadSummary) => {
    setDragPayload(isSelected(t.thread_id) ? [...selected] : [t.thread_id]);
  };

  const clearGroupDrag = () => {
    setDragPayload(null);
    setGroupDropTarget(null);
    setDropOverThread(null);
  };

  /** 该会话当前所在的组（多对多，取第一个；"拖出"只从它所在的框里移除）。 */
  const groupOf = (id: string) => groupsForThread(id)[0] ?? null;

  /**
   * 拖到组框上：批量加入该组。
   *
   * 已在组里的跳过（幂等）——重发一次仍是一次多余的写，且会让组内顺序抖动。
   * 跨 workspace 的落点直接忽略：拖拽的失败反馈应当是"没反应"，而不是一条
   * 打断操作的红字。
   */
  const dropIntoGroup = (workspace: string, group: SessionGroup) => {
    const ids = dragPayload;
    clearGroupDrag();
    if (!ids || ids.length === 0) return;
    const add = ids.filter(
      (id) => workspaceOfThread(id) === workspace && !group.threadIds.includes(id),
    );
    if (add.length === 0) return;
    void onSetGroupMembers(workspace, group.id, [...group.threadIds, ...add]);
  };

  /**
   * 拖到框外**会话行**上：批量移出。
   *
   * 目标组 = `dragstart` 那一行所在的第一个组：多选跨越两个组时，从 A 组拖出来
   * 只解散 A 组那批，B 组的成员不动——"拖出这个框"就是它字面的意思。
   *
   * 落点只看会话行，目录标题与空白处都不认（它们不是会话，当落点等于让用户猜）。
   * 落点是哪一行也不影响结果：顺序仍由 `updated_at` 决定，本次不做排序。
   */
  const dropOutOfGroup = () => {
    const ids = dragPayload;
    clearGroupDrag();
    if (!ids || ids.length === 0) return;
    const current = groupOf(ids[0]);
    if (!current) return;
    const drop = new Set(ids.filter((id) => current.group.threadIds.includes(id)));
    if (drop.size === 0) return;
    void onSetGroupMembers(
      current.workspace,
      current.group.id,
      current.group.threadIds.filter((x) => !drop.has(x)),
    );
  };
```

同时删掉第 602-609 行的 `addableFor` 整个函数，以及第 300 行的 `addingTo` 状态（`const [addingTo, setAddingTo] = useState<string | null>(null);` 及其上方注释）。

- [ ] **Step 4: 更新所有 `dragThread` 引用**

- 组框的 `onDragOver/onDragLeave/onDrop`（第 681-702 行）：`dragThread === null` 改 `dragPayload === null`；`dropIntoGroup(workspace, group)` 调用不变。
- 项目的 `onDragOver`（第 1441-1451 行）：整块删除（目录标题不再是落点），只保留 `onDrop` 里的 `e.preventDefault()`？——**不保留**：这行 `onDrop` 也要删，否则目录标题仍会接住 drop。删掉 `onDragOver` 与 `onDrop` 两个属性。
- 所有 `setDragThread(null)` / `setGroupDropTarget(null)` 的 `onDragEnd` 收尾（第 911-921 行、第 1063-1067 行）改为 `clearGroupDrag()`。注意置顶行的 `onDragEnd` 还要保留 `setDragId(null); setDropIndex(null);`。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd desktop && node_modules/.bin/tsc --noEmit && node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx
```

预期：PASS。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): 进出分组支持整批拖拽，移出落点收窄为会话行"
```

---

### Task 3: 框外会话行作为「移出」落点并高亮

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx:896-925`（`renderThread` 的行属性）
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 `dropOverThread`、`dropOutOfGroup()`。
- Produces: 会话行新增 `data-group-drop-target` 标记（`dropOverThread === t.thread_id` 时），供测试与样式使用。

- [ ] **Step 1: 写失败测试**

在 `describe("拖动会话进组")` 里追加：

```tsx
  it("拖到框外会话行上时该行高亮", () => {
    const { container } = renderSidebar({ groups: dragBase });
    fireEvent.click(screen.getByLabelText("展开分组 登录模块"));
    const row = container.querySelector<HTMLElement>('[data-thread-row="a"]')!;
    const outside = container.querySelector<HTMLElement>('[data-thread-row="c"]')!;
    fireEvent.dragStart(row, { dataTransfer: { setData: vi.fn() } });
    fireEvent.dragOver(outside, { dataTransfer: { dropEffect: "" } });
    expect(container.querySelector('[data-group-drop-target="c"]')).toBeTruthy();
  });

  it("离开落点后高亮熄灭", () => {
    const { container } = renderSidebar({ groups: dragBase });
    fireEvent.click(screen.getByLabelText("展开分组 登录模块"));
    const row = container.querySelector<HTMLElement>('[data-thread-row="a"]')!;
    const outside = container.querySelector<HTMLElement>('[data-thread-row="c"]')!;
    fireEvent.dragStart(row, { dataTransfer: { setData: vi.fn() } });
    fireEvent.dragOver(outside, { dataTransfer: { dropEffect: "" } });
    fireEvent.dragEnd(row, { dataTransfer: { setData: vi.fn() } });
    expect(container.querySelector("[data-group-drop-target]")).toBeNull();
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx -t "拖到框外会话行上时该行高亮"
```

预期：FAIL，找不到 `[data-group-drop-target="c"]`。

- [ ] **Step 3: 给会话行加落点属性**

在 `renderThread` 里 `const dataAttr = ...`（第 891 行）之后插入：

```tsx
    // 「移出分组」的落点只认会话行：拖动一批组内成员时，悬停到框外任意一行
    // 都能松手移出。置顶行不做落点——它自己的拖拽是重排，两套语义别混。
    const canAcceptGroupDrop = dragPayload !== null && !dragging;
```

JSX 里同一个属性名不能写两遍，所以**整体替换**第 894-923 行（从 `draggable={...}` 到 `onDragEnd` 结束）为下面这块——置顶分支与移出分支在这里合流：

```tsx
        draggable={dragging ? editingId !== t.thread_id : canDragIntoGroup}
        {...(canAcceptGroupDrop && dropOverThread === t.thread_id
          ? { "data-group-drop-target": t.thread_id }
          : {})}
        onDragStart={
          dragging
            ? () => setDragId(t.thread_id)
            : canDragIntoGroup
              ? () => onGroupDragStart(t)
              : undefined
        }
        onDragOver={
          // 置顶行：重排落点。其它行：移出分组落点。两者互斥（`dragging` 与
          // `canAcceptGroupDrop` 不会同时为真）。
          dragging
            ? (e) => {
                if (dragId === null) return;
                // 必须 preventDefault，否则浏览器不认这个落点（也不发 drop）。
                e.preventDefault();
                setDropIndex(opts.dragIndex ?? null);
              }
            : canAcceptGroupDrop
              ? (e) => {
                  // 拖着的东西压根不在任何组里时，哪儿都不是「移出」落点，
                  // 否则整片侧栏都会亮起来而松手无事发生。
                  const ids = dragPayload;
                  if (!ids || !groupOf(ids[0])) return;
                  e.preventDefault();
                  e.dataTransfer.dropEffect = "move";
                  setDropOverThread(t.thread_id);
                }
              : undefined
        }
        onDragLeave={
          canAcceptGroupDrop
            ? () => setDropOverThread((cur) => (cur === t.thread_id ? null : cur))
            : undefined
        }
        onDrop={
          dragging
            ? (e) => {
                e.preventDefault();
                commitPinDrop(opts.dragIndex ?? 0);
              }
            : canAcceptGroupDrop
              ? (e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  dropOutOfGroup();
                }
              : undefined
        }
        onDragEnd={() => {
          setDragId(null);
          setDropIndex(null);
          clearGroupDrag();
        }}
```

注意 Task 2 已经把 `onDragEnd` 改成 `clearGroupDrag()`，这里是它加上置顶收尾之后的最终形态；照着整块替换即可，不要只贴一半。

- [ ] **Step 4: 加高亮样式**

把第 985 行的 className 模板里插入一段：

```tsx
        } ${dropOverThread === t.thread_id ? "ring-1 ring-amber-400 ring-inset" : ""} ${
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd desktop && node_modules/.bin/tsc --noEmit && node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx
```

预期：PASS。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): 框外会话行作为移出分组的落点并高亮"
```

---

### Task 4: 删掉「+」入口，改写相关测试

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx:760-800`（组框头的 `+` 与候选列表）
- Test: `desktop/src/components/ThreadSidebar.test.tsx:1968`（`describe("组框：加入会话入口")`）

**Interfaces:**
- Consumes: Task 2 已删 `addableFor`、`addingTo`。
- Produces: 组框头只剩「展开/折叠」「组名」「成员数」三个元素。

- [ ] **Step 1: 改写测试**

把 `describe("组框：加入会话入口")` 整个块替换为：

```tsx
describe("组框：加入会话只走拖拽", () => {
  const base: WorkspaceGroup[] = [
    {
      workspace: "/work/projA",
      exists: true,
      threads: [
        thread("a", "A", "/work/projA"),
        thread("b", "B", "/work/projA"),
        thread("c", "C", "/work/projA"),
      ],
      groups: [{ id: "g1", name: "登录模块", description: "", threadIds: ["a", "b"] }],
    },
  ];

  it("组框头不再有「+」按钮与候选列表", () => {
    // 「+」把「把会话放进组」表达成一次菜单选择，实测不直观；拖拽才是用户
    // 脑子里的那个动作。同一件事只留一个入口。
    const { container } = renderSidebar({ groups: base });
    expect(container.querySelector("[data-group-add]")).toBeNull();
    expect(container.querySelector("[data-group-add-picker]")).toBeNull();
    expect(container.querySelector("[data-add-candidate]")).toBeNull();
  });

  it("组框头显示成员数", () => {
    renderSidebar({ groups: base });
    expect(document.querySelector("[data-group-count]")?.textContent).toBe("2");
  });

  it("新建组成功后自动展开新框", async () => {
    const onCreateGroup = vi.fn().mockResolvedValue("g-new");
    const { container, rerender } = renderSidebar({
      groups: [{ workspace: "/work/projA", exists: true, threads: base[0].threads }],
      onCreateGroup,
    });
    fireEvent.click(screen.getByText("A"), { metaKey: true, detail: 1 });
    fireEvent.click(screen.getByText("B"), { metaKey: true, detail: 2 });
    fireEvent.click(screen.getByText("新建组（2）"));
    const input = document.querySelector<HTMLInputElement>("[data-new-group-name]")!;
    fireEvent.change(input, { target: { value: "登录模块" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(onCreateGroup).toHaveBeenCalled());

    rerender(
      <ThreadSidebar
        {...sidebarProps({
          groups: [
            {
              workspace: "/work/projA",
              exists: true,
              threads: base[0].threads,
              groups: [{ id: "g-new", name: "登录模块", description: "", threadIds: ["a", "b"] }],
            },
          ],
          onCreateGroup,
        })}
      />,
    );
    expect(container.querySelector('[data-thread-row="a"]')).toBeTruthy();
    expect(container.querySelector('[data-thread-row="b"]')).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx -t "组框：加入会话只走拖拽"
```

预期：FAIL，`[data-group-add]` 仍存在。

- [ ] **Step 3: 删掉按钮与候选列表**

删除第 760-800 行的注释块与 `{/* 「加入会话」入口... */}`、`<button data-group-add ...>+</button>`、`{addingTo === group.id && (...)}` 整块。组框头保留 `data-group-count` 的 `<span>`。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && node_modules/.bin/tsc --noEmit && node_modules/.bin/vitest run
```

预期：全量 PASS（约 1631 + 新增用例）。

- [ ] **Step 5: 变异验证（确认测试真的钉住了行为）**

```bash
cd desktop
# 变异 1：把 modifierDown 从 canDragIntoGroup 里删掉 → 测试 2 应变红
# 变异 2：把 dropIntoGroup 的 add 改回只取 ids[0] → 批量用例应变红
```

分别在编辑器里改这两处、各跑一次 `node_modules/.bin/vitest run src/components/ThreadSidebar.test.tsx`，确认变红后**改回**。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): 删掉组框头的 + 入口，加入会话只走拖拽"
```

---

### Task 5: 登记项目进度

**Files:**
- Modify: `docs/project-management/desktop.md`

**Interfaces:**
- Consumes: 前四个任务的最终实现。
- Produces: 一条带可验证判据的 `[x]` 条目。

- [ ] **Step 1: 在会话分组小节追加条目**

```markdown
- [x] 会话分组：拖拽进出（替换「+」按钮）— 整行可拖，按 Cmd/Ctrl/Shift 时让位给点击多选；多选时拖任意一条 = 整批进出；移出落点只认会话行。判据：`cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`（「无修饰键时普通会话行整行可拖」「按着 Cmd 时整行不可拖」「选中三条后拖其中一条进组，整批三条一起加」「三条都在组内时拖其中一条到框外会话行，三条一起移出」「多选跨两个组时，从 A 组拖出只解散 A 组那批」「组框头不再有「+」按钮与候选列表」六例；把 `modifierDown` 从 `canDragIntoGroup` 删掉、或把批量改回单条即失败）
```

- [ ] **Step 2: 跑全量验证**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit && node_modules/.bin/vitest run
```

预期：全绿，用例数不少于改动前的 1631。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md
git commit -m "docs: 登记会话分组拖拽进出"
```

---

## Self-Review

**Spec coverage**

| Spec 条目 | 对应任务 |
| --- | --- |
| 拖到组框 → 加入 | Task 2 |
| 从组框拖到框外 → 移出 | Task 2、Task 3 |
| 多选整批 | Task 2 |
| 删「+」按钮与选择器 | Task 4 |
| 整行可拖 + 修饰键让位 | Task 1 |
| payload 快照规则 | Task 2 |
| 跨 workspace 忽略 | Task 2（`workspaceOfThread(id) === workspace` 过滤） |
| 移出目标组 = dragstart 行第一个组 | Task 2（`groupOf(ids[0])`） |
| 落点只认会话行 | Task 2（删项目组头的 onDragOver/onDrop）、Task 3 |
| 高亮 | Task 2（框）、Task 3（行） |
| 不做排序 | 全局约束；无任务改动顺序 |
| 测试清单 1-8 | Task 1/2/3/4 逐条覆盖 |

**类型一致性**：`dragPayload: string[] | null`、`onGroupDragStart(t: ThreadSummary)`、`dropOutOfGroup()`（无参）、`dropOverThread: string | null` 在 Task 2/3 中用法一致。

**占位符扫描**：无 TBD/TODO；每个代码步骤含实际代码。
