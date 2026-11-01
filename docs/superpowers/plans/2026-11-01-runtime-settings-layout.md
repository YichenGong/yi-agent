# 运行参数页排版重做 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把桌面端「设置 → 运行参数」从一堵无样式的 60 行堆叠，改成与其它设置页同语言的「分组卡片 + 单行字段 + 吸底操作条」。

**Architecture:** 只改 `SettingsRuntimeTab.tsx` 一个组件的 JSX 与样式类名，外加 `index.css` 一条移动端字号选择器补 `textarea`。所有视觉常量（组名、组说明、枚举中文映射）都放在同文件的纯函数/常量表里，不新增模块、不新增依赖。断点用**容器查询** `@lg:`，因为设置弹窗内容区宽度上限只有 568px（见设计 §3.1）。

**Tech Stack:** React 19 + TypeScript + Tailwind CSS v4.3.3 + Vite + Vitest（jsdom）

## Global Constraints

- 设计文档：`docs/superpowers/specs/2026-11-01-runtime-settings-layout-design.md`（以下简称「设计」）。所有决策以它为准。
- **不改后端**：`settings/read|write` 协议、`runtimeSettings.ts` 的 `parseRuntimeSettings` / `buildPatch` **一行不动**。
- **不改三态语义**：`value === null` 仍是「未设置」，绝不归一成默认值。
- **不新增依赖、不新增模块、不新增 CSS 文件**。
- `index.css` **只允许改一处**：给移动端 16px 选择器补 `textarea`（设计 §3.11）。
- 可用颜色 token **仅限**：`surface` / `panel` / `raised` / `line` / `line-strong` / `fg` / `fg-muted` / `fg-subtle` / `fg-faint`。
  **禁止** `bg-base` / `bg-accent` / `text-accent`——它们在构建产物里不存在（设计 §1.7）。
- 断点**只能用容器查询** `@lg:`（32rem = 512px），容器 `@container` 挂在**组卡片**上。**禁止** `sm:` / `md:` 视口断点（设计 §3.1）。
- 本仓库**没有** `@testing-library/jest-dom`，断言用原生 DOM 属性（`x.disabled === true`、`x.className.includes(...)`），**不要**用 `toBeDisabled()` / `toHaveTextContent()`。
- 提交前在 `desktop/` 下跑 `npx tsc --noEmit`；只提交相关文件。
- commit message 用 conventional commits，**不要**写 `Co-Authored-By`。

## File Structure

| 文件 | 职责 | 本次动作 |
|---|---|---|
| `desktop/src/components/SettingsRuntimeTab.tsx` | 「运行参数」页全部 UI | **重写 JSX + 新增常量表** |
| `desktop/src/components/SettingsRuntimeTab.test.tsx` | 该页测试 | 改 2 条、加强 1 条、新增 5 条 |
| `desktop/src/index.css` | 移动端 16px 字号选择器 | 补 `textarea`（唯一一处） |
| `desktop/src/components/SettingsDialog.test.tsx` | 设置弹窗集成 | 仅在「读取中…」文案受影响时改（本计划不改文案，预期无需动） |
| `docs/project-management/desktop.md` | 项目进度 | 加一条 `[x]` 记录 |

`SettingsRuntimeTab.tsx` 会从 154 行涨到约 300 行。仍是一个文件：它是一个页面的完整 UI，拆开会让「组名 + 枚举映射 + 渲染」分散在三个文件里，反而更难读。

---

### Task 1: 补 `index.css` 的移动端 textarea 字号规则

**Files:**
- Modify: `desktop/src/index.css:433-435`
- Test: `desktop/src/mobileLayout.test.ts:273-282`（既有断言，**不改**，用来证明没破坏它）

**Interfaces:**
- Produces: `[data-mobile="true"] .settings-panel textarea` 命中 `font-size: 16px`。Task 4 的 `<textarea>` 依赖它。

- [ ] **Step 1: 改选择器**

把 `desktop/src/index.css` 中这段（约 433 行）：

```css
[data-mobile="true"] .settings-panel input,
[data-mobile="true"] .settings-panel select {
  font-size: 16px;
}
```

改成：

```css
[data-mobile="true"] .settings-panel input,
[data-mobile="true"] .settings-panel select,
[data-mobile="true"] .settings-panel textarea {
  font-size: 16px;
}
```

同时把上方注释里的「设置面板内的输入控件字号下限」一句保留原样，只在其后补一句说明：

```css
 * 设置面板内的输入控件字号下限：面板里的输入框是 `text-sm`（14px），而 iOS 在聚焦
 * 字号 < 16px 的输入框时会自动放大页面——`lockZoom` 只能禁掉用户的手动捏合缩放，
 * 挡不住这条自动放大；放大后页面不再贴合屏幕，之后每次滑动都变成两轴平移。
 * 16px 是 iOS 不触发自动放大的下限，与侧栏搜索框那条（见上）同源、同理由。
 * `textarea` 一并收进来：运行参数页的「系统提示词」用的就是它。
 * 桌面端不匹配 `[data-mobile]`，逐字节不变。
```

- [ ] **Step 2: 跑既有断言，确认没被破坏**

Run: `cd desktop && npx vitest run src/mobileLayout.test.ts`
Expected: PASS（23 例全绿）。`mobileLayout.test.ts:273` 的正则是 `/(?:input|select)/`，新选择器仍匹配。

- [ ] **Step 3: 提交**

```bash
cd desktop
git add src/index.css
git commit -m "style(desktop): 移动端 16px 字号规则覆盖 textarea"
```

---

### Task 2: 常量表与两个纯函数

把组名、组说明、枚举中文映射做成模块级常量，把「标签查找」做成纯函数。**先写测试再写实现。**

**Files:**
- Modify: `desktop/src/components/SettingsRuntimeTab.tsx:9-15`（替换现有 `GROUP_LABELS` / `GROUP_ORDER`）
- Test: `desktop/src/components/SettingsRuntimeTab.test.tsx`（新增 describe 块）

**Interfaces:**
- Produces:
  - `GROUP_LABELS: Record<string, string>` — `agent`/`compact`/`startup`/`tui` 四个中文名
  - `GROUP_HINTS: Record<string, string>` — 四组各一句说明
  - `GROUP_ORDER: string[]` — `["agent", "compact", "tui", "startup"]`
  - `VALUE_LABELS: Record<string, Record<string, string>>` — 四个枚举字段的中文映射
  - `labelFor(key: string, raw: string): string` — 查不到原样返回，空串返回 `"—"`

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/components/SettingsRuntimeTab.test.tsx` 末尾（`describe` 块外）新增：

```tsx
import { GROUP_HINTS, GROUP_LABELS, GROUP_ORDER, labelFor } from "./SettingsRuntimeTab";

describe("runtime settings labels", () => {
  it("names every backend group in Chinese", () => {
    // 与 yi-agent-runtime/src/settings.rs 的 FIELDS 分组一一对应。漏一个就会在页面上
    // 露出英文组名（tui 曾经就是这样）。
    expect(GROUP_ORDER).toEqual(["agent", "compact", "tui", "startup"]);
    for (const group of GROUP_ORDER) {
      expect(GROUP_LABELS[group]).toBeTruthy();
      expect(GROUP_HINTS[group]).toBeTruthy();
    }
  });

  it("translates enum values but keeps unknown ones verbatim", () => {
    expect(labelFor("startup.sandbox", "workspace-write")).toBe("工作区可写");
    expect(labelFor("tui.notification_method", "bel")).toBe("响铃");
    // 后端将来加选项时，前端不该留空或抛错。
    expect(labelFor("startup.sandbox", "some-future-mode")).toBe("some-future-mode");
    // 没有映射表的字段（数字）原样返回。
    expect(labelFor("agent.max_turns", "200")).toBe("200");
    // `default_display` 可能是空串（agent.system_prompt），显示成破折号。
    expect(labelFor("agent.system_prompt", "")).toBe("—");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: FAIL — `GROUP_HINTS` / `labelFor` 未导出（`SyntaxError` 或 `undefined`）。

- [ ] **Step 3: 写实现**

把 `desktop/src/components/SettingsRuntimeTab.tsx:9-15` 整段替换为：

```tsx
export const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "上下文压缩",
  startup: "启动",
  tui: "终端通知",
};

/** 每组一句话说明在管什么。后端新增分组时查不到就不渲染，不造空话。 */
export const GROUP_HINTS: Record<string, string> = {
  agent: "单次对话的执行上限与提示词覆盖",
  compact: "上下文接近上限时的自动压缩策略",
  tui: "命令行界面在回合结束时的提示方式",
  startup: "启动时生效的默认档位",
};

/** 按「影响面从大到小」排：agent 是主循环，compact 是它的兜底，tui 是另一个前端。 */
export const GROUP_ORDER = ["agent", "compact", "tui", "startup"];

/**
 * 枚举值的显示文案。**只影响显示**：提交给 `settings/write` 的永远是原始字符串。
 *
 * 不在此表里的字段（数字、自由文本）原样返回——这张表是「补充说明」而不是白名单，
 * 后端将来加选项时前端不至于留空。
 */
const VALUE_LABELS: Record<string, Record<string, string>> = {
  "startup.sandbox": {
    "read-only": "只读",
    "workspace-write": "工作区可写",
    "danger-full-access": "完全访问",
  },
  "tui.notifications": { true: "开", false: "关" },
  "tui.notification_method": { auto: "自动", osc9: "OSC 9（带正文）", bel: "响铃" },
  "tui.notification_condition": { unfocused: "仅窗口失焦时", always: "总是" },
};

/**
 * 字段值的中文显示。空串显示成破折号——它出现两处：`agent.system_prompt` 的
 * `default_display`（默认就是「不覆盖」），以及用户清空输入框后的草稿值。
 */
export function labelFor(key: string, raw: string): string {
  if (raw === "") return "—";
  return VALUE_LABELS[key]?.[raw] ?? raw;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: PASS（6 例：原有 4 + 新增 2）。

- [ ] **Step 5: 提交**

```bash
cd desktop
git add src/components/SettingsRuntimeTab.tsx src/components/SettingsRuntimeTab.test.tsx
git commit -m "feat(desktop): 运行参数页补齐组名与枚举中文映射"
```

---

### Task 3: 错误信息改用 `formatError`

`String(error)` 会把服务端 `RpcError` 对象渲染成 `[object Object]`（设计 §1.6）。

**Files:**
- Modify: `desktop/src/components/SettingsRuntimeTab.tsx:45`、`:76`
- Test: `desktop/src/components/SettingsRuntimeTab.test.tsx`（加强既有那条）

**Interfaces:**
- Consumes: `formatError(e: unknown): string`，来自 `../lib/errorMessage`（已存在，勿改）
- Produces: 无新接口

- [ ] **Step 1: 加强既有测试**

把 `desktop/src/components/SettingsRuntimeTab.test.tsx` 里最后一条测试（`reports the server's rejection inline instead of a whole-page error`）替换为下面两条：

```tsx
  it("reports the server's rejection inline instead of a whole-page error", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") return { fields: [field({ value: 500 })] };
      throw new Error("invalid setting agent.max_turns: must be at least 1");
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = await screen.findByLabelText("最大轮次");
    fireEvent.change(input, { target: { value: "0" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByText(/must be at least 1/)).toBeTruthy();
  });

  it("renders an RpcError object as its message, not [object Object]", async () => {
    // RPC 失败时 reject 的是 `{code, message, data?}` 对象（lib/rpc.ts 的
    // `pending.reject(msg.error)`）。`String(obj)` 得 "[object Object]"。
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") return { fields: [field({ value: 500 })] };
      throw { code: -32602, message: "invalid setting agent.max_turns: must be at least 1" };
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = await screen.findByLabelText("最大轮次");
    fireEvent.change(input, { target: { value: "0" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByText(/must be at least 1/)).toBeTruthy();
    expect(screen.queryByText(/\[object Object\]/)).toBeNull();
  });
```

- [ ] **Step 2: 跑测试确认新用例失败**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: 新用例 FAIL（页面上是 `[object Object]`，匹配不到 message）。

- [ ] **Step 3: 改实现**

在 `desktop/src/components/SettingsRuntimeTab.tsx` 顶部加导入：

```tsx
import { formatError } from "../lib/errorMessage";
```

把 `load` 的 catch 里（原 `:45`）：

```tsx
      setNotice({ ok: false, text: String(error) });
```

改为：

```tsx
      setNotice({ ok: false, text: formatError(error) });
```

把 `save` 的 catch 里（原 `:76`）：

```tsx
      // 服务端把 `invalid_setting` 的原因放在 message 里，直接内联显示。
      setNotice({ ok: false, text: String(error) });
```

改为：

```tsx
      // 服务端把 `invalid_setting` 的原因放在 message 里，直接内联显示。
      // 必须先过 formatError：RPC 失败给的是 `{code, message}` 对象，
      // 直接 String() 会渲染成 "[object Object]"。
      setNotice({ ok: false, text: formatError(error) });
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: PASS（7 例）。

- [ ] **Step 5: 提交**

```bash
cd desktop
git add src/components/SettingsRuntimeTab.tsx src/components/SettingsRuntimeTab.test.tsx
git commit -m "fix(desktop): 运行参数页错误信息不再渲染成 [object Object]"
```

---

### Task 4: 重写渲染：分组卡片 + 单行字段 + 吸底操作条

这是本次的主体。**先改测试再改实现**，分三个小步走，每步都能独立跑绿。

**Files:**
- Modify: `desktop/src/components/SettingsRuntimeTab.tsx:82-154`（整个渲染部分）
- Test: `desktop/src/components/SettingsRuntimeTab.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 `GROUP_LABELS` / `GROUP_HINTS` / `GROUP_ORDER` / `labelFor`；Task 3 的 `formatError`
- Produces: DOM 约定——保存按钮可访问名 `"保存"`、撤销按钮 `"撤销改动"`、恢复按钮 `"恢复默认"`、每个字段控件 `id="runtime-<key>"` 且被同名 `<label>` 关联。

- [ ] **Step 1: 改 2 条旧断言（未设置态）**

把 `desktop/src/components/SettingsRuntimeTab.test.tsx` 里第一条测试替换为：

```tsx
  it("shows the default and offers no restore button when the value is unset", async () => {
    render(<SettingsRuntimeTab call={callWith([field()])} />);
    // 未设置：只显示「默认 200」，不出现「已设为」。
    expect(await screen.findByText("默认 200")).toBeTruthy();
    expect(screen.queryByText(/已设为/)).toBeNull();
    // 「恢复默认」在无需恢复时**不渲染**——12 颗禁用按钮正是原来最难看的噪声。
    expect(screen.queryByRole("button", { name: /恢复默认/ })).toBeNull();
  });
```

把第二条测试替换为：

```tsx
  it("marks a customised value and offers restore when a value exists", async () => {
    render(<SettingsRuntimeTab call={callWith([field({ value: 500 })])} />);
    expect(await screen.findByText("已设为 500")).toBeTruthy();
    expect(screen.queryByText("默认 200")).toBeNull();
    const restore = screen.getByRole("button", { name: /恢复默认/ }) as HTMLButtonElement;
    // 本仓库没有 @testing-library/jest-dom，故用原生 DOM property。
    expect(restore.disabled).toBe(false);
  });
```

- [ ] **Step 2: 跑测试确认这 2 条失败**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: 这 2 条 FAIL（旧实现输出「未设置 · 默认 200」且恒定渲染禁用按钮）。

- [ ] **Step 3: 先补 state、路径与辅助函数（页面仍是旧 JSX，这两条测试仍红）**

在 `SettingsRuntimeTab` 组件内、`const [fields, setFields] = useState<RuntimeField[]>([]);` 之后加一行：

```tsx
  const [path, setPath] = useState("");
```

把 `load` 里的：

```tsx
      const view = parseRuntimeSettings(await call("settings/read", {}));
      setFields(view.fields);
      // 草稿从「当前值」起步，让 buildPatch 能算出用户改了哪一项。
      const next: Record<string, string> = {};
      for (const field of view.fields) {
        next[field.key] = field.value === null ? "" : String(field.value);
      }
      setDrafts(next);
```

改为：

```tsx
      const view = parseRuntimeSettings(await call("settings/read", {}));
      setFields(view.fields);
      // `path` 只在读成功时更新：它是页脚的上下文，读失败时保留上一次的值，
      // 免得底栏闪一下空白。
      setPath(view.path);
      // 草稿从「当前值」起步，让 buildPatch 能算出用户改了哪一项。
      setDrafts(draftsOf(view.fields));
```

在组件外（`labelFor` 之后）加一个纯函数：

```tsx
/** 草稿的起点：每个字段取「当前值」，未设置取空串。 */
function draftsOf(fields: RuntimeField[]): Record<string, string> {
  const next: Record<string, string> = {};
  for (const field of fields) {
    next[field.key] = field.value === null ? "" : String(field.value);
  }
  return next;
}
```

在 `save` 函数之后加：

```tsx
  /** 「撤销改动」：草稿回到各字段的当前值，`buildPatch` 随之变空、保存按钮重新禁用。 */
  const resetDrafts = () => {
    setDrafts(draftsOf(fields));
    setNotice(null);
  };
```

- [ ] **Step 4: 替换渲染（页面骨架 + 分组 + 单行字段 + 吸底条）**

本步给每个字段渲染一个**通用 `<input>`**（数值字段 `type="number"`）。这不是临时占位：没有控件，本步 Step 6 的脏状态测试就找不到输入框，Task 4 也就无法独立跑绿。`select` 与 `textarea` 的差异化渲染留给 Task 5。

把 `desktop/src/components/SettingsRuntimeTab.tsx:82-154` 替换为：

```tsx
  if (loading) return <p className="p-5 text-sm text-fg-subtle">读取中…</p>;

  const patch = buildPatch(fields, drafts);
  const dirty = Object.keys(patch).length > 0;

  if (fields.length === 0) {
    return <p className="p-5 text-sm text-fg-subtle">没有可配置的运行参数</p>;
  }

  return (
    <div className="flex min-h-full flex-col">
      <div className="flex-1 space-y-6 p-5">
        {notice ? (
          <p
            role={notice.ok ? "status" : "alert"}
            className={`rounded-md border px-3 py-2 text-xs ${
              notice.ok
                ? "border-line bg-raised/50 text-fg-muted"
                : "border-amber-500/40 bg-amber-500/10 text-amber-500"
            }`}
          >
            {notice.text}
          </p>
        ) : null}

        {grouped.map(({ group, fields: rows }) => (
          <section key={group}>
            <h3 className="text-sm font-medium text-fg">{GROUP_LABELS[group] ?? group}</h3>
            {GROUP_HINTS[group] ? (
              <p className="mt-1 text-xs text-fg-subtle">{GROUP_HINTS[group]}</p>
            ) : null}
            <div className="@container mt-3 divide-y divide-line rounded-md border border-line">
              {rows.map((field) => {
                const isUnset = field.value === null;
                return (
                  <div
                    key={field.key}
                    className="flex flex-col gap-2 px-3 py-2.5 @lg:flex-row @lg:items-start @lg:justify-between @lg:gap-6"
                  >
                    <div className="min-w-0 @lg:flex-1">
                      <label
                        htmlFor={`runtime-${field.key}`}
                        className="block text-sm text-fg"
                      >
                        {field.label}
                      </label>
                      <p className="mt-0.5 text-xs leading-relaxed text-fg-subtle">
                        {field.description}
                      </p>
                    </div>
                    <div className="shrink-0 @lg:w-56">
                      {/* 先统一渲染 <input>：本步的脏状态测试需要真实控件存在，
                          否则 Task 4 无法独立跑绿。select / textarea 在 Task 5 升级。 */}
                      <input
                        id={`runtime-${field.key}`}
                        type={field.type === "number" ? "number" : "text"}
                        min={field.min ?? undefined}
                        max={field.max ?? undefined}
                        value={drafts[field.key] ?? ""}
                        onChange={(event) =>
                          setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                        }
                        className="w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle disabled:opacity-50"
                      />
                      <div className="mt-1 flex items-center justify-between gap-2 text-xs">
                        <span className={isUnset ? "text-fg-subtle" : "text-fg-muted"}>
                          {isUnset
                            ? `默认 ${labelFor(field.key, field.default)}`
                            : `已设为 ${labelFor(field.key, String(field.value))}`}
                        </span>
                        {isUnset ? null : (
                          <button
                            type="button"
                            disabled={saving}
                            onClick={() => void save({ [field.key]: null })}
                            className="rounded text-fg-subtle hover:text-fg disabled:opacity-50"
                          >
                            恢复默认
                          </button>
                        )}
                      </div>
                    </div>
                  </div>
                );
              })}
            </div>
          </section>
        ))}
      </div>

      <div className="sticky bottom-0 flex flex-wrap items-center justify-between gap-3 border-t border-line bg-panel px-5 py-3">
        <span className="min-w-0 truncate text-xs text-fg-subtle" title={path}>
          settings.json · {path}
        </span>
        <div className="flex shrink-0 items-center gap-2">
          <button
            type="button"
            disabled={saving || !dirty}
            onClick={() => resetDrafts()}
            className="rounded border border-line-strong px-3 py-1.5 text-sm text-fg-muted disabled:opacity-50"
          >
            撤销改动
          </button>
          <button
            type="button"
            disabled={saving || !dirty}
            onClick={() => void save(patch)}
            className="rounded bg-fg px-3 py-1.5 text-sm text-surface disabled:opacity-50"
          >
            {saving ? "保存中…" : "保存"}
          </button>
        </div>
      </div>
    </div>
  );
```

- [ ] **Step 5: 跑测试确认这 2 条通过**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: 这 2 条 PASS。其余 `sends null to restore the default` 与 `reports the server's rejection...` 也应仍 PASS（它们点的是「恢复默认」与「保存」，两个按钮都还在）。

- [ ] **Step 6: 写新增测试（脏状态 / 路径 / 无改动）**

在 `describe("SettingsRuntimeTab", ...)` 块内追加：

```tsx
  it("disables save until something changes, and undo restores the baseline", async () => {
    render(<SettingsRuntimeTab call={callWith([field({ value: 500 })])} />);
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    const saveBtn = screen.getByRole("button", { name: /保存/ }) as HTMLButtonElement;
    const undoBtn = screen.getByRole("button", { name: /撤销改动/ }) as HTMLButtonElement;
    // 刚加载时没有改动，两颗按钮都点不动。
    expect(saveBtn.disabled).toBe(true);
    expect(undoBtn.disabled).toBe(true);

    fireEvent.change(input, { target: { value: "800" } });
    expect(saveBtn.disabled).toBe(false);
    expect(undoBtn.disabled).toBe(false);

    fireEvent.click(undoBtn);
    expect(input.value).toBe("500");
    expect(saveBtn.disabled).toBe(true);
    expect(undoBtn.disabled).toBe(true);
  });

  it("shows the settings.json path from the server", async () => {
    render(<SettingsRuntimeTab call={callWith([field()])} />);
    expect(await screen.findByText(/\/x\/settings\.json/)).toBeTruthy();
  });

  it("renders the tui group under a Chinese heading", async () => {
    const tuiField = field({
      key: "tui.notification_method",
      group: "tui",
      label: "通知协议",
      type: "select",
      options: ["auto", "osc9", "bel"],
      default: "auto",
      description: "auto 按终端自动选择；osc9 带正文，bel 只响铃",
      min: null,
    });
    render(<SettingsRuntimeTab call={callWith([tuiField])} />);
    expect(await screen.findByText("终端通知")).toBeTruthy();
    // 绝不能漏出英文组名——那是 `GROUP_LABELS[group] ?? group` 的老问题。
    expect(screen.queryByText("tui")).toBeNull();
  });
```

- [ ] **Step 7: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: PASS（10 例）。

- [ ] **Step 8: 提交**

```bash
cd desktop
git add src/components/SettingsRuntimeTab.tsx src/components/SettingsRuntimeTab.test.tsx
git commit -m "feat(desktop): 运行参数页改为分组卡片与吸底操作条"
```

---

### Task 5: 控件升级（select / textarea）

Task 4 已让每行都有一个可用的 `<input>`，本步只做两件事：`type === "select"` 改渲染 `<select>`（中文文案、原始值提交），`agent.system_prompt` 改渲染多行 `<textarea>`。因此本步结束时那 2 条新测试才从「字段值看起来对但控件类型不对」转为通过。

**Files:**
- Modify: `desktop/src/components/SettingsRuntimeTab.tsx`（把 Task 4 的通用 `<input>` 升级为按 `field.type` 分支的 select / textarea）
- Test: `desktop/src/components/SettingsRuntimeTab.test.tsx`

**Interfaces:**
- Consumes: `labelFor`（Task 2）
- Produces: DOM 约定——`select` 的 `option` 显示中文但 `value` 是原始字符串；`agent.system_prompt` 渲染 `<textarea>`。

- [ ] **Step 1: 写失败的测试（枚举中文 + 原值提交）**

追加：

```tsx
  it("shows enum options in Chinese but submits the raw value", async () => {
    const sandbox = field({
      key: "startup.sandbox",
      group: "startup",
      label: "默认沙箱档位",
      type: "select",
      options: ["read-only", "workspace-write", "danger-full-access"],
      default: "workspace-write",
      description: "启动时的文件写入沙箱；空表示用默认档位",
      min: null,
    });
    const call = callWith([sandbox]);
    render(<SettingsRuntimeTab call={call} />);

    // 默认值本身也要中文化，否则下拉里是中文、旁边写着英文。
    expect(await screen.findByText("默认 工作区可写")).toBeTruthy();

    const select = (await screen.findByLabelText("默认沙箱档位")) as HTMLSelectElement;
    const labels = Array.from(select.options).map((o) => o.textContent);
    expect(labels).toContain("只读");
    expect(labels).toContain("完全访问");
    // 未设置项显示默认值的中文。
    expect(labels[0]).toBe("未设置（默认 工作区可写）");

    // 选中文项，提交的仍是原始字符串。
    fireEvent.change(select, { target: { value: "danger-full-access" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("settings/write", {
        patch: { "startup.sandbox": "danger-full-access" },
      });
    });
  });

  it("uses a multi-line textarea for the free-text system prompt", async () => {
    const prompt = field({
      key: "agent.system_prompt",
      label: "系统提示词",
      type: "text",
      default: "",
      description: "覆盖默认系统提示词；留空表示用默认",
      min: null,
    });
    render(<SettingsRuntimeTab call={callWith([prompt])} />);
    const box = (await screen.findByLabelText("系统提示词")) as HTMLElement;
    expect(box.tagName).toBe("TEXTAREA");
    // 空默认值显示成破折号，不是空白。
    expect(screen.getByText("默认 —")).toBeTruthy();
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: 两条新增都 FAIL。此时每行渲染的是 Task 4 的通用 `<input>`，所以 `findByLabelText("默认沙箱档位")` 拿到的不是 `<select>`（`select.options` 为 undefined），`系统提示词` 那一行的 `tagName` 是 `INPUT` 而不是 `TEXTAREA`。

- [ ] **Step 3: 实现三种控件**

把 Task 4 里每个字段那一段**单独的 `<input>`**（连同它上面那两行注释）：

```tsx
                      {/* 先统一渲染 <input>：本步的脏状态测试需要真实控件存在，
                          否则 Task 4 无法独立跑绿。select / textarea 在 Task 5 升级。 */}
                      <input
                        id={`runtime-${field.key}`}
                        type={field.type === "number" ? "number" : "text"}
                        min={field.min ?? undefined}
                        max={field.max ?? undefined}
                        value={drafts[field.key] ?? ""}
                        onChange={(event) =>
                          setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                        }
                        className="w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle disabled:opacity-50"
                      />
```

**第 1 步：先加常量。** 在组件外（`draftsOf` 旁边）加：

```tsx
/**
 * 控件的外观。三个控件共用一份，避免 select / textarea / input 之间漂移。
 *
 * `bg-raised` + `border-line-strong` 而非 `bg-surface` + `border-line`：本页卡片底色
 * 是 `panel`，#0a0a0a 的 `surface` 落在 #171717 上是一块更黑的「洞」，边框也几乎
 * 看不见；`raised` 比 `panel` 亮一档，控件才读作「凸起的可编辑区」。
 */
const CONTROL_CLASS =
  "w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle disabled:opacity-50";
```

**第 2 步：再把上面那段单独的 `<input>` 替换为这个三分支：**

```tsx
                      {field.type === "select" ? (
                        <select
                          id={`runtime-${field.key}`}
                          value={drafts[field.key] ?? ""}
                          onChange={(event) =>
                            setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                          }
                          className={CONTROL_CLASS}
                        >
                          <option value="">
                            未设置（默认 {labelFor(field.key, field.default)}）
                          </option>
                          {field.options.map((option) => (
                            <option key={option} value={option}>
                              {labelFor(field.key, option)}
                            </option>
                          ))}
                        </select>
                      ) : field.key === "agent.system_prompt" ? (
                        <textarea
                          id={`runtime-${field.key}`}
                          rows={3}
                          value={drafts[field.key] ?? ""}
                          onChange={(event) =>
                            setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                          }
                          className={CONTROL_CLASS}
                        />
                      ) : (
                        <input
                          id={`runtime-${field.key}`}
                          type={field.type === "number" ? "number" : "text"}
                          min={field.min ?? undefined}
                          max={field.max ?? undefined}
                          value={drafts[field.key] ?? ""}
                          onChange={(event) =>
                            setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                          }
                          className={CONTROL_CLASS}
                        />
                      )}
```

`textarea` 额外加 `resize-y` 会与 `CONTROL_CLASS` 冲突吗？不冲突——`resize-y` 是独立工具类。故 `className` 写成：

```tsx
                          className={`${CONTROL_CLASS} resize-y`}
```

（把上面 textarea 那行的 `className={CONTROL_CLASS}` 换成这行。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx`
Expected: PASS（12 例）。

- [ ] **Step 5: 提交**

```bash
cd desktop
git add src/components/SettingsRuntimeTab.tsx src/components/SettingsRuntimeTab.test.tsx
git commit -m "feat(desktop): 运行参数页控件统一外观与中文枚举"
```

---

### Task 6: 类型检查、构建、产物类名门禁

前三项是自动化，第四项必须**看着输出**逐条确认——设计 §5.4 要求每个新类名都真的生成了。

**Files:**
- 无源码改动（本任务只验证；若发现问题则回到对应 Task 修）

- [ ] **Step 1: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无输出（exit 0）。

- [ ] **Step 2: 跑本页 + 相关测试**

Run: `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx src/components/SettingsDialog.test.tsx src/lib/runtimeSettings.test.ts src/mobileLayout.test.ts src/lib/themeTokens.test.ts`
Expected: 全绿。`themeTokens.test.ts` 会扫全部源码禁止 `neutral-<数字>` 字面色阶——本计划用的是语义 token，应通过。

- [ ] **Step 3: 构建**

Run: `cd desktop && npm run build`
Expected: `✓ built in ...`，无类型错误。

- [ ] **Step 4: 产物类名门禁（逐条 grep，必须全部命中）**

Run:

```bash
cd desktop
CSS=$(ls -t dist/assets/*.css | head -1)
echo "CSS=$CSS"
for c in '@container' 'divide-y' 'divide-line' 'bg-raised' 'border-line-strong' \
         'min-h-full' 'sticky' 'truncate' 'leading-relaxed' \
         'focus\:border-fg-subtle' 'disabled\:opacity-50' \
         'bg-amber-500\/10' 'border-amber-500\/40' 'resize-y'; do
  printf "%-28s %s\n" "$c" "$(grep -c "$c" "$CSS")"
done
echo "--- 必须为 0（本页不得使用这些不存在的 token）---"
for c in 'bg-base' 'bg-accent' 'text-accent'; do
  printf "%-28s %s\n" "$c" "$(grep -c "\.$c[{,: ]" "$CSS")"
done
```

Expected: 上半部分每一行**非 0**；下半部分每一行**为 0**。

若某个类名是 0：说明该类名写错了或 Tailwind 没扫到，回到对应 Task 修正后重跑本步。

- [ ] **Step 5: 确认容器查询阈值是 512px 而不是其它**

Run: `cd desktop && grep -o '@container (width >= [0-9]*rem)' $(ls -t dist/assets/*.css | head -1) | sort -u`
Expected: 输出包含 `@container (width >= 32rem)`（= 512px）。若只有 `28rem`，说明写成了 `@md:`，回到 Task 4 改成 `@lg:`。

- [ ] **Step 6: 提交（仅当本任务改动过文件）**

若 Step 1-5 全程无改动，跳过本步。

---

### Task 7: 视觉自检（深色 / 浅色、宽 / 窄）

**这一步不可省。** 设计 §3.1 与 §3.5 的两个关键决策（容器查询阈值、控件配色）都是靠截图推翻过初版的，机器断言查不出「难看」。

**Files:**
- 临时文件（**验证后删除，不提交**）：`desktop/probe-runtime.html`、`desktop/probe-runtime.css`

- [ ] **Step 1: 生成 mock 页**

在 `desktop/` 下建 `probe-runtime.html`：

```html
<!DOCTYPE html><html lang="zh" data-theme="dark"><head><meta charset="utf-8"></head>
<body class="bg-surface text-fg">
<div class="flex overflow-hidden rounded-lg border border-line" style="height:100vh">
  <nav class="w-40 shrink-0 border-r border-line bg-surface p-2 text-sm text-fg-subtle">
    通用<br>运行参数<br>远程访问<br>插件<br>模型
  </nav>
  <div class="min-w-0 flex-1 overflow-y-auto">
    <div class="flex min-h-full flex-col">
      <div class="flex-1 space-y-6 p-5">
        <section>
          <h3 class="text-sm font-medium text-fg">Agent</h3>
          <p class="mt-1 text-xs text-fg-subtle">单次对话的执行上限与提示词覆盖</p>
          <div class="@container mt-3 divide-y divide-line rounded-md border border-line">
            <div class="flex flex-col gap-2 px-3 py-2.5 @lg:flex-row @lg:items-start @lg:justify-between @lg:gap-6">
              <div class="min-w-0 @lg:flex-1">
                <span class="block text-sm text-fg">最大轮次</span>
                <p class="mt-0.5 text-xs leading-relaxed text-fg-subtle">一次对话里 agent 最多执行的轮数</p>
              </div>
              <div class="shrink-0 @lg:w-56">
                <input type="number" class="w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle">
                <div class="mt-1 flex items-center justify-between gap-2 text-xs"><span class="text-fg-subtle">默认 200</span></div>
              </div>
            </div>
            <div class="flex flex-col gap-2 px-3 py-2.5 @lg:flex-row @lg:items-start @lg:justify-between @lg:gap-6">
              <div class="min-w-0 @lg:flex-1">
                <span class="block text-sm text-fg">系统提示词</span>
                <p class="mt-0.5 text-xs leading-relaxed text-fg-subtle">覆盖默认系统提示词；留空表示用默认</p>
              </div>
              <div class="shrink-0 @lg:w-56">
                <textarea rows="3" class="w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle resize-y"></textarea>
                <div class="mt-1 flex items-center justify-between gap-2 text-xs"><span class="text-fg-muted">已设为 500</span><button class="rounded text-fg-subtle hover:text-fg">恢复默认</button></div>
              </div>
            </div>
          </div>
        </section>
        <section>
          <h3 class="text-sm font-medium text-fg">终端通知</h3>
          <p class="mt-1 text-xs text-fg-subtle">命令行界面在回合结束时的提示方式</p>
          <div class="@container mt-3 divide-y divide-line rounded-md border border-line">
            <div class="flex flex-col gap-2 px-3 py-2.5 @lg:flex-row @lg:items-start @lg:justify-between @lg:gap-6">
              <div class="min-w-0 @lg:flex-1">
                <span class="block text-sm text-fg">通知协议</span>
                <p class="mt-0.5 text-xs leading-relaxed text-fg-subtle">auto 按终端自动选择；osc9 带正文，bel 只响铃</p>
              </div>
              <div class="shrink-0 @lg:w-56">
                <select class="w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle">
                  <option>未设置（默认 自动）</option><option>自动</option><option>OSC 9（带正文）</option><option>响铃</option>
                </select>
                <div class="mt-1 flex items-center justify-between gap-2 text-xs"><span class="text-fg-subtle">默认 自动</span></div>
              </div>
            </div>
          </div>
        </section>
      </div>
      <div class="sticky bottom-0 flex flex-wrap items-center justify-between gap-3 border-t border-line bg-panel px-5 py-3">
        <span class="min-w-0 truncate text-xs text-fg-subtle">settings.json · /Users/x/.yi-agent/settings.json</span>
        <div class="flex shrink-0 items-center gap-2">
          <button class="rounded border border-line-strong px-3 py-1.5 text-sm text-fg-muted opacity-50">撤销改动</button>
          <button class="rounded bg-fg px-3 py-1.5 text-sm text-surface opacity-50">保存</button>
        </div>
      </div>
    </div>
  </div>
</div>
</body></html>
```

生成浅色版：`sed 's/data-theme="dark"/data-theme="light"/' probe-runtime.html > probe-runtime-light.html`

- [ ] **Step 2: 生成 CSS 并注入**

```bash
cd desktop
W=$(mktemp -d)
cp index.css "$W/base.css"
printf '\n@source "./probe-runtime.html";\n@source "./probe-runtime-light.html";\n' >> "$W/base.css"
npx @tailwindcss/cli -i "$W/base.css" -o "$W/out.css"
python3 - "$W" <<'PY'
import sys, pathlib
w = pathlib.Path(sys.argv[1]); css = (w/"out.css").read_text()
for n in ("probe-runtime", "probe-runtime-light"):
    p = pathlib.Path(n + ".html"); h = p.read_text()
    assert "</head>" in h
    pathlib.Path(n + ".built.html").write_text(h.replace("</head>", f"<style>{css}</style></head>"))
print("ok")
PY
```

- [ ] **Step 3: 截图四个组合**

```bash
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
cd desktop
"$CHROME" --headless --disable-gpu --hide-scrollbars --virtual-time-budget=1500 --window-size=1280,860 --screenshot=/tmp/rt-dark-wide.png file://$PWD/probe-runtime.built.html
"$CHROME" --headless --disable-gpu --hide-scrollbars --virtual-time-budget=1500 --window-size=900,600  --screenshot=/tmp/rt-dark-narrow.png file://$PWD/probe-runtime.built.html
"$CHROME" --headless --disable-gpu --hide-scrollbars --virtual-time-budget=1500 --window-size=1280,860 --screenshot=/tmp/rt-light-wide.png file://$PWD/probe-runtime-light.built.html
"$CHROME" --headless --disable-gpu --hide-scrollbars --virtual-time-budget=1500 --window-size=900,600  --screenshot=/tmp/rt-light-narrow.png file://$PWD/probe-runtime-light.built.html
ls -la /tmp/rt-*.png
```

- [ ] **Step 4: 逐张看图，核对这 6 条**

对四张图逐条确认（用 `view_image`）：

1. 组标题与组说明之间、组与组之间有明显留白，不是一堵墙。
2. 宽窗口（1280）下控件左边缘**对齐成一条竖线**；窄窗口（900）下**自动堆叠成单列**（不是把标签挤成 3 行——那说明断点写错了）。
3. 输入框看起来是**从卡片上浮起来**的，不是一块更黑的洞。
4. 未设置的那行只有「默认 X」，**没有**「恢复默认」按钮；已设置的那行有。
5. 底栏贴在面板底部（内容不足时也不上浮），路径被截断且不换行。
6. 浅色主题下没有任何「浅字压浅底」的失读处；主按钮反色（深底浅字 / 浅底深字）。

任何一条不满足 → 回到 Task 4/5 修，重跑本 Task。

- [ ] **Step 5: 清理临时文件（不提交）**

```bash
cd desktop
rm -f probe-runtime.html probe-runtime-light.html probe-runtime.built.html probe-runtime-light.built.html
rm -f /tmp/rt-*.png
git status --short   # 应只剩预期的源码改动
```

- [ ] **Step 6: 提交（仅当 Step 4 触发了修复）**

若 Step 4 未发现需要修复的问题，跳过。

---

### Task 8: 更新项目进度文档

**Files:**
- Modify: `docs/project-management/desktop.md`（在设置相关条目附近追加一条）

**Interfaces:**
- Produces: 一条带可验证判据的 `[x]` 记录

- [ ] **Step 1: 追加条目**

在 `docs/project-management/desktop.md` 中「手机端设置界面改全屏页 + 顶部横排 Tab」那条之后，插入：

```markdown
- [x] 「运行参数」页排版重做（原先整页没有样式类：`settings-runtime`/`settings-group`/`settings-row`/`settings-hint`/`settings-error` 在全部 CSS 里都不存在，页面退回浏览器默认排版；每字段 5 层竖叠、placeholder 与提示重复、12 颗常驻禁用「恢复默认」按钮）— `desktop/src/components/SettingsRuntimeTab.tsx`：分组卡片（标题 + 一句话说明 + `rounded-md border border-line` + `divide-y divide-line`）、单字段一行（左标签+描述 / 右 `@lg:w-56` 控件）、状态合并为「默认 X」或「已设为 X + 行内恢复默认」、吸底操作条（`settings.json` 真实路径 + 撤销改动 + 反色保存）、`tui` 组补中文名「终端通知」并排进 `GROUP_ORDER`、枚举中文显示（`labelFor`，提交仍是原始字符串）；`desktop/src/index.css:433` 移动端 16px 字号选择器补 `textarea`（「系统提示词」用多行输入）。**断点用容器查询 `@lg:`（512px）而非视口断点**：设置弹窗内容区上限只有 `min(70vw,768)-200 = 568px`，`md:`(768) 量的是视口、在 900px 窗口下会把左侧标签挤成 3 行（真实 Chrome 实测）。顺带修掉 `String(error)` 把 RPC 错误对象渲染成 `[object Object]`（改用 `lib/errorMessage.ts` 的 `formatError`）。判据：`cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx src/components/SettingsDialog.test.tsx src/lib/runtimeSettings.test.ts src/mobileLayout.test.ts` + `npx tsc --noEmit` + `npm run build`，且构建产物 CSS 中本页所用类名逐个命中、`bg-base`/`bg-accent`/`text-accent` 命中数为 0 — [设计](../superpowers/specs/2026-11-01-runtime-settings-layout-design.md)、[计划](../superpowers/plans/2026-11-01-runtime-settings-layout.md)
```

- [ ] **Step 2: 验证判据里点名的命令确实全绿**

Run:

```bash
cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx src/components/SettingsDialog.test.tsx src/lib/runtimeSettings.test.ts src/mobileLayout.test.ts
```

Expected: 全绿。若某个文件红，说明文档写早了——修完再提交。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md
git commit -m "docs(project-management): 记录运行参数页排版重做"
```

---

### Task 9: 全量回归

单页改动也可能波及全局（`themeTokens.test.ts` 扫全源码、`SettingsDialog.test.tsx` 渲染整个弹窗）。

**Files:**
- 无源码改动

- [ ] **Step 1: 前端全量**

Run: `cd desktop && npx vitest run`
Expected: 全绿，且用例总数 = 改动前的基线 + 8（Task 2 加 2、Task 3 加 1、Task 4 加 3、Task 5 加 2）。

- [ ] **Step 2: 类型与构建**

Run: `cd desktop && npx tsc --noEmit && npm run build`
Expected: 均无输出/成功。

- [ ] **Step 3: 汇报**

把 Task 6 的类名门禁输出、Task 9 的用例总数一并写进交付说明。**不要**在没跑完的情况下声称通过。

---

## Self-Review

**1. 设计覆盖检查**

| 设计章节 | 对应任务 |
|---|---|
| §2.1 目标 1（同语言 padding/边框/控件尺寸） | Task 5（`CONTROL_CLASS`）、Task 4 Step 4（`p-5` / `border-line`） |
| §2.1 目标 2（一字段一行） | Task 4 Step 4（`@lg:flex-row`） |
| §2.1 目标 3（层级 + 恢复按钮只在可操作时出现） | Task 4 Step 1（测试）、Step 4（`isUnset ? null`） |
| §2.1 目标 4（底部操作区 + 路径 + 无改动不可保存） | Task 4 Step 4、Step 6 |
| §2.1 目标 5（组名全中文 + tui 位置） | Task 2 |
| §2.1 目标 6（枚举中文、原始值提交） | Task 2、Task 5 |
| §2.1 目标 7（错误可读） | Task 3 |
| §2.2 非目标（后端/三态/其它页/依赖/index.css 只一处） | Global Constraints 逐条约束；Task 1 只补 `textarea` |
| §3.1 容器查询断点 `@lg:` | Task 4 Step 4（`@container` + `@lg:*`）、Task 6 Step 5（产物 32rem） |
| §3.2 页面骨架 `min-h-full flex-col` | Task 4 Step 4 |
| §3.3 组名/组说明/顺序 | Task 2 |
| §3.4 单行字段 | Task 4 Step 4 |
| §3.5 `bg-raised` + `border-line-strong` | Task 5 Step 3（`CONTROL_CLASS`）、Task 7 Step 4 第 3 条 |
| §3.6 状态与消除重复 | Task 4 Step 1（测试）、Step 4（实现） |
| §3.7 枚举中文映射 | Task 2、Task 5 |
| §3.8 吸底操作条（含路径） | Task 4 Step 4、Step 6 |
| §3.9 notice + `formatError` | Task 3、Task 4 Step 4 |
| §3.10 空列表 | Task 4 Step 4（`fields.length === 0` 分支） |
| §3.11 textarea + index.css | Task 1、Task 5 |
| §3.12 数据流不变 | Global Constraints（`runtimeSettings.ts` 一行不动）、Task 4 Step 3（`load` 只改取草稿方式） |
| §4 测试（改 2 / 加强 1 / 新增 5） | Task 4 Step 1（改 2）、Task 3 Step 1（加强 1 + 新增 1）、Task 4 Step 6（新增 3）、Task 5 Step 1（新增 2）、Task 2 Step 1（纯函数 2） |
| §5 验证 1-6 | Task 6、7、8、9 |
| §6 风险（逐条） | 见下表 |

**§6 风险逐条落点**

| 风险 | 计划中的应对 |
|---|---|
| 断点用错（`sm:`/`md:`） | Global Constraints 明令禁用；Task 6 Step 5 断言产物含 `32rem` |
| 旧 WebView 不支持容器查询 | 降级为单栏堆叠，不破版（无额外任务） |
| 文案改动破坏测试 | Task 4 Step 1 先改断言；Task 9 全量回归 |
| 误用不存在的 token | Global Constraints 白名单；Task 6 Step 4 双向 grep |
| 控件像黑洞 | Task 5 Step 3 + Task 7 Step 4 第 3 条 |
| `fg-subtle` 对比度 | 刻意与全站一致（无任务，属既定 token） |
| 触屏 textarea 未覆盖 | Task 1 |
| 保存后文案变化 | Task 3 只换 `formatError`，成功文案不变 |

无遗漏。

**2. 占位符扫描**

已逐条检查：无 TBD / TODO /「类似 Task N」/「适当处理错误」。每个代码步骤都给了可直接粘贴的完整代码。

原稿这里有一处**真占位**，已修：Task 4 Step 4 的 JSX 曾写 `{/* 控件在 Step 4 填入 */}`，
而 Task 4 Step 6 的「脏状态」测试需要输入框才能 `findByLabelText("最大轮次")`。
照原稿执行，Task 4 **不可能**跑绿——`label` 的 `htmlFor` 指向一个不存在的控件，
Testing Library 直接报 "no form control was found associated to that label"。

修法：Task 4 就给每个字段渲染一个通用 `<input>`（数值字段 `type="number"`），
Task 5 再按 `field.type` 升级成 `<select>` / `<textarea>`。这样两个 Task 各自都能独立跑绿。
已实测确认：Task 4 结束时 7 例全绿，Task 5 结束时 12 例全绿。

**3. 类型一致性**

- `labelFor(key: string, raw: string): string` — Task 2 定义，Task 4/5 均以 `labelFor(field.key, ...)` 调用，参数顺序一致。
- `draftsOf(fields: RuntimeField[]): Record<string, string>` — Task 4 Step 4 定义，`load` 与 `resetDrafts` 两处调用一致。
- `CONTROL_CLASS` — Task 5 Step 3 定义为模块级常量，同 Task 内三处引用。
- `path` state — Task 4 Step 3 引入，Task 4 Step 4 的底栏引用（`title={path}` / `{path}`）。顺序已排正：先加 state 与辅助函数（Step 3），再替换 JSX（Step 4），任一步都不会出现「JSX 引用未定义变量」的中间态。
- `GROUP_LABELS` / `GROUP_HINTS` / `GROUP_ORDER` — 均需 `export`，Task 5 的 `describe("runtime settings labels")` 从组件导入它们做断言。

**4. 实跑验证（2026-11-01）**

不是纸面自检：把本计划的全部代码（Task 1 的 CSS、Task 2 的常量与纯函数、Task 4 的
state 与渲染、Task 5 的测试）**实际写进组件跑了一遍**，验证完再还原。结果：

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 新测试 | `npx vitest run src/components/SettingsRuntimeTab.test.tsx` | 12 passed |
| 类型 | `npx tsc --noEmit` | exit 0，无输出 |
| 全量回归 | `npx vitest run` | 138 files / 1832 tests passed |
| jsdom 行为 | `fireEvent.click` 打在 `disabled` 按钮上 | 不触发 onClick（脏状态测试的前提成立） |

第二轮按 Task 顺序逐步实跑（Task 2 → 3 → 4 → 5），过程中查出并修正 **4 处真问题**：

1. **三个常量缺 `export`** — 测试从组件导入 `GROUP_LABELS` / `GROUP_HINTS` /
   `GROUP_ORDER` 做断言，不导出则整份测试文件编译不过。已在 Task 2 补上。
2. **Task 4 的 Step 3 与 Step 4 顺序颠倒** — 原顺序先写 JSX 再补 `path` /
   `resetDrafts`，第一步就引用未定义变量。已改为「先 state 与辅助函数，再换 JSX」。
3. **Task 4 的控件是占位，导致该 Task 不可能跑绿** — 详见上一节。已改为 Task 4
   渲染通用 `<input>`、Task 5 升级为 select / textarea。
4. **Task 5 的 Files 栏写错了定位** — 原文说「修改 Task 4 留下的占位」，改法不明。
   现明确为「把 Task 4 的通用 `<input>` 升级为按 `field.type` 分支」。

分阶段实测结果（每步都真跑，不是纸面推断）：

| 阶段 | 用例数 | 结果 |
| --- | --- | --- |
| 基线（改动前） | 1824 | 全绿 |
| Task 4 结束 | 7（本文件） | 全绿 |
| Task 5 结束 | 12（本文件） | 全绿 |
| 全部完成后全量 | 1832 | 138 files 全绿 |

`1824 + 8 = 1832`，与 Task 9 断言的新增用例数吻合。全量回归证明本方案不碰
`App.test.tsx` 里既有的设置页断言。
