# 会话侧栏滚动条删除按钮修复实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 防止 macOS 会话列表滚动条覆盖会话行的 Delete 按钮，同时保留滚动条和原有滚动行为。

**Architecture:** 侧栏滚动容器保持全宽且继续负责垂直滚动，只在容器内部包裹可滚动内容的行列表并添加右侧安全间距。组件回归测试检查内容包裹层、滚动容器关系及 Delete 回调行为。

**Tech Stack:** React 19、TypeScript、Tailwind CSS v4、Vitest、Testing Library。

**Status:** 已实现并合并。代码提交 `87932b9e`（fix(desktop): reserve scrollbar-safe padding in the session list，merge `e7cd9e01`），Task 1 全部步骤完成。

## Global Constraints

- 滚动条保持可见，滚动行为不变。
- 安全间距仅应用于滚动列表内容，不改变 New thread 和桌面搜索栏布局。
- 手机搜索框继续与会话列表处于同一滚动容器。
- 不引入依赖，不修改删除语义或按钮显隐规则。

---

### Task 1: 预留会话列表滚动条安全间距并增加回归测试

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`（列表滚动容器内包裹列表内容）
- Test: `desktop/src/components/ThreadSidebar.test.tsx`（新增布局回归断言；现有 Delete 回调测试覆盖删除行为）

**Interfaces:**
- Consumes: `ThreadSidebar` 当前列表结构、`[data-group-row]` 行标记、`overflow-y-auto` 容器和现有 `onDelete` prop。
- Produces: 列表内标记为 `[data-thread-list-content]` 的内容包裹层，带 `pr-4` 安全间距；会话行仍位于该滚动容器内部。

- [x] **Step 1: 写布局回归测试**

在 `describe("ThreadSidebar 搜索")` 的现有滚动容器测试旁新增测试：

```tsx
it("为滚动会话内容预留右侧滚动条安全间距", () => {
  const { container } = renderSidebar();
  const row = container.querySelector("[data-group-row]")!;
  const content = row.closest("[data-thread-list-content]");
  const scroller = row.closest(".overflow-y-auto");

  expect(content).not.toBeNull();
  expect(content!.className.split(/\s+/)).toContain("pr-4");
  expect(content!.parentElement).toBe(scroller);
});
```

- [x] **Step 2: 运行测试确认 RED**

运行：`cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx -t "为滚动会话内容预留右侧滚动条安全间距"`

预期：测试失败，因为当前列表没有 `[data-thread-list-content]` 包裹层；失败不是测试执行错误。

- [x] **Step 3: 实现最小列表内容包裹层**

在 `ThreadSidebar.tsx` 的 `<div className="flex-1 overflow-y-auto">` 内，保持移动端 `searchBar` 在滚动容器中的原位置，并把空态提示、置顶列表及分组列表包在下面的容器里：

```tsx
{isMobile && searchBar}
<div data-thread-list-content="" className="pr-4">
  {/* 原有空态、Pinned 区域及 filtered.groups 渲染，不改其逻辑 */}
</div>
```

桌面端 `searchBar` 在滚动容器之外的现状保持不变。`pr-4` 让行内按钮与滚动容器右缘之间留出 1rem 安全区，滚动容器自身宽度和滚动条不变。

- [x] **Step 4: 运行相关测试确认 GREEN**

运行：`cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`

预期：整个 ThreadSidebar 测试文件通过，包括新增安全间距测试、现有桌面/手机滚动容器断言与现有“长按会话行的「删除」调用 onDelete”测试。

- [x] **Step 5: 运行前端生产构建**

运行：`cd desktop && npm run build`

预期：TypeScript 检查和 Vite 生产构建成功。

- [x] **Step 6: 检查最终差异**

运行：`git diff --check && git diff -- desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx`

预期：无空白错误；差异仅包括列表内容包装、安全间距和针对该布局的回归测试。
