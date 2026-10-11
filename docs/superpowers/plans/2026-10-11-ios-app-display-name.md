# iOS 主屏应用显示名（`Yi-Agent`）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 iPhone 主屏 app 图标下方的名字从 `yi-agent` 变成 `Yi-Agent`。

**Architecture:** 不改 `PRODUCT_NAME`（保住可执行文件名 / `.app` / `.ipa` / dSYM 路径仍是 `yi-agent`），只新增 `CFBundleDisplayName = Yi-Agent`。因主屏名解析优先级是 `CFBundleDisplayName` → `CFBundleName`（= `$(PRODUCT_NAME)`），加前者即可改显示名。改动同时落在生成源 `project.yml` 与已提交的生成产物 `desktop_iOS/Info.plist`，并加一条读真实产物的静态门禁测试。

**Tech Stack:** Tauri 2 iOS 工程（xcodegen 生成的 `desktop.xcodeproj`）、Apple `Info.plist`、Vitest（node 环境，读真实文件做门禁）。

## Global Constraints

- **禁止**改 `PRODUCT_NAME`（`desktop/src-tauri/gen/apple/project.yml` 的 `app` settingGroup 与 `desktop.xcodeproj/project.pbxproj` 里都是 `yi-agent`）；可执行文件名 / `.app` / `.ipa` / dSYM 路径必须保持 `yi-agent`（`docs/relay-deploy.md` §4.2 的构建、安装命令依赖它们）。
- 显示名字面值为 **`Yi-Agent`**（`Y`、`i`、`-`、`A`、`g`，连字符，无空格）。
- `project.yml` 是 `desktop.xcodeproj` 与 `desktop_iOS/Info.plist` 的**生成源**：跑 `npx tauri ios init` 会按它重生成工程。凡是该文件能派生的配置，必须同时写进 `project.yml`，否则下次 init 被抹掉（见 `docs/relay-deploy.md` §4.2 坑 2）。
- 所有变更在 worktree `.worktrees/fix-ios-display-name` 的分支 `fix/ios-app-display-name` 上完成；**不得**在 `main` 上直接改或提交。
- 提交前在 worktree 内跑 `cd desktop && npx vitest run` 与 `npx tsc --noEmit`。
- commit message 用 conventional commits 风格，**不要**写 `Co-Authored-By: Claude` 行。

---

## File Structure

- `desktop/src-tauri/gen/apple/project.yml` —— 修改：`desktop_iOS.targets.info.properties` 下新增 `CFBundleDisplayName`（生成源，防回退）。
- `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist` —— 修改：新增 `CFBundleDisplayName`（本次构建实际读取的产物）。
- `desktop/src/iosDisplayName.test.ts` —— 新建：读真实 `Info.plist` 与 `project.yml` 的静态门禁测试。
- `docs/relay-deploy.md` —— 修改：§4 记录主屏显示名。
- `docs/project-management/desktop.md` —— 修改：iOS target 条目补记显示名。

---

### Task 1: 新增 `CFBundleDisplayName = Yi-Agent`（含静态门禁测试）

**Files:**
- Create: `desktop/src/iosDisplayName.test.ts`
- Modify: `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist:13-14`
- Modify: `desktop/src-tauri/gen/apple/project.yml:58`

**Interfaces:**
- Consumes: 无（首任务）。
- Produces: `CFBundleDisplayName = Yi-Agent` 同时出现在 `desktop_iOS/Info.plist` 与 `project.yml` 的 `info.properties`；测试文件 `desktop/src/iosDisplayName.test.ts` 中含两个用例（分别钉住这两处）。

- [ ] **Step 1: 写失败的静态门禁测试**

新建 `desktop/src/iosDisplayName.test.ts`，内容逐字如下（照抄 `mobileLayout.test.ts` 的 `import.meta.dirname` 读真实产物写法）：

```ts
/** @vitest-environment node */
import { describe, it, expect } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

/**
 * iOS 主屏应用显示名的静态门禁。
 *
 * 直接读**真实**的 `Info.plist` 与它的生成源 `project.yml`，断言的是真正会打包
 * 进 app 的产物，而不是测试里另写一份常量（与 `mobileLayout.test.ts` 同一套路）。
 *
 * 为什么用 `CFBundleDisplayName` 而不是改 `PRODUCT_NAME`：主屏名解析优先级是
 * `CFBundleDisplayName` → `CFBundleName`（= `$(PRODUCT_NAME)`）。只加前者就能改
 * 显示名，同时保住可执行文件名 / `.app` / `.ipa` / dSYM 路径仍是 `yi-agent`
 * （`docs/relay-deploy.md` 记录的构建安装命令依赖它们）。
 */
const dir = String((import.meta as unknown as { dirname: string }).dirname);
const infoPlist = () =>
  readFileSync(`${dir}/../src-tauri/gen/apple/desktop_iOS/Info.plist`, "utf8");
const projectYml = () =>
  readFileSync(`${dir}/../src-tauri/gen/apple/project.yml`, "utf8");

describe("iOS home-screen display name", () => {
  it("sets CFBundleDisplayName to Yi-Agent in the built Info.plist", () => {
    expect(infoPlist()).toMatch(
      /<key>CFBundleDisplayName<\/key>\s*<string>Yi-Agent<\/string>/,
    );
  });

  it("also sets it in project.yml so `tauri ios init` cannot drop it", () => {
    // project.yml 是 desktop.xcodeproj 与 desktop_iOS/Info.plist 的生成源：只改
    // Info.plist 的话，跑一次 `npx tauri ios init` 就被重生成抹掉。
    expect(projectYml()).toMatch(/^\s*CFBundleDisplayName:\s*Yi-Agent\s*$/m);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/iosDisplayName.test.ts`
Expected: FAIL —— 两个用例都失败（`Info.plist` / `project.yml` 里都还没这个键）。

- [ ] **Step 3: 在 `Info.plist` 加键**

编辑 `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist`，在 `CFBundleName` 那两行之后插入：

```xml
	<key>CFBundleDisplayName</key>
	<string>Yi-Agent</string>
```

改完后该片段应为（`CFBundleName` 与新增键相邻，缩进用 tab，与全文件一致；文件末尾无换行，保持不变）：

```xml
	<key>CFBundleName</key>
	<string>$(PRODUCT_NAME)</string>
	<key>CFBundleDisplayName</key>
	<string>Yi-Agent</string>
	<key>CFBundlePackageType</key>
```

- [ ] **Step 4: 在 `project.yml` 加键**

编辑 `desktop/src-tauri/gen/apple/project.yml`，在 `desktop_iOS.targets.info.properties` 的 `CFBundleVersion: "0.1.0"` 之后（8 空格缩进）插入一行：

```yaml
        CFBundleDisplayName: Yi-Agent
```

改完后该片段应为：

```yaml
        CFBundleShortVersionString: 0.1.0
        CFBundleVersion: "0.1.0"
        CFBundleDisplayName: Yi-Agent
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/iosDisplayName.test.ts`
Expected: PASS —— 2 passed。

- [ ] **Step 6: 全量前端测试 + 类型检查**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: 全绿，无类型错误。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/iosDisplayName.test.ts \
        desktop/src-tauri/gen/apple/desktop_iOS/Info.plist \
        desktop/src-tauri/gen/apple/project.yml
git commit -m "feat(desktop): show Yi-Agent under the iOS home-screen icon"
```

---

### Task 2: 同步文档

**Files:**
- Modify: `docs/relay-deploy.md`（§4.1 / §4.2 之间或末尾加一条说明）
- Modify: `docs/project-management/desktop.md`（iOS target 条目，`desktop.md:62`）

**Interfaces:**
- Consumes: Task 1 的 `CFBundleDisplayName = Yi-Agent`。
- Produces: 无（文档）。

- [ ] **Step 1: 记录 relay-deploy 的显示名**

在 `docs/relay-deploy.md` §4.1「工程位置」段落末尾，追加一句（与既有中文、`file:line` 站位的风格一致）：

```
主屏显示名由 `CFBundleDisplayName` 决定（`project.yml` 的 `info.properties` + 已提交的
`desktop_iOS/Info.plist`，两处都写，值 `Yi-Agent`）；`PRODUCT_NAME` 保持 `yi-agent`，
故可执行文件名 / `.app` / `.ipa` / dSYM 路径不变，§4.2 的构建与安装命令仍适用。
```

- [ ] **Step 2: 更新 project-management/desktop.md**

编辑 `docs/project-management/desktop.md:62` 那条 iOS target 记录的末尾，在既有「部署与限制见 [relay-deploy](../relay-deploy.md)」之前插入：

```
**主屏显示名** 为 `Yi-Agent`：`CFBundleDisplayName` 写在生成源 `desktop/src-tauri/gen/apple/project.yml` 的 `info.properties` 与产物 `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist` 两处（`PRODUCT_NAME` 仍为 `yi-agent`，产物路径不变）；静态门禁 `desktop/src/iosDisplayName.test.ts`，验证 `cd desktop && npx vitest run src/iosDisplayName.test.ts`。
```

（保持该行是单个 markdown 列表项，不要新起段落断行。）

- [ ] **Step 3: 提交**

```bash
git add docs/relay-deploy.md docs/project-management/desktop.md
git commit -m "docs: record the iOS home-screen name (Yi-Agent)"
```

---

### Task 3: 合并回 `main`（收尾）

**Files:** 无（git 操作）。

- [ ] **Step 1: 最终验证**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: 全绿。

- [ ] **Step 2: 按 finishing-a-development-branch 合并**

回仓库根：`git merge --no-ff fix/ios-app-display-name`，随后删除分支 `fix/ios-app-display-name` 与移除 worktree `.worktrees/fix-ios-display-name`（具体步骤依 `superpowers:finishing-a-development-branch`）。

- [ ] **Step 3（可选，需真机 + 签名）: 端到端确认**

按 `docs/relay-deploy.md` §4.2 重打包安装后，肉眼确认 iPhone 主屏图标下方显示 `Yi-Agent`（自动化无法覆盖真实主屏渲染，故列为可选）。

---

## Self-Review

- **Spec coverage：** spec 的实现清单 1（project.yml）→ Task 1 Step 4；2（Info.plist）→ Task 1 Step 3；3（静态门禁测试）→ Task 1 Step 1；4（文档同步 relay-deploy + project-management）→ Task 2；5（worktree + 分支）→ 已在写作计划前完成，收尾 Task 3。范围外项（macOS 桌面名、PRODUCT_NAME、图标资源）本计划均未触碰。
- **Placeholder scan：** 无 TBD / TODO；测试与配置代码均为可直接落盘的逐字内容。
- **Type consistency：** 测试里断言的键名/字面值（`CFBundleDisplayName`、`Yi-Agent`）与 Task 1 Step 3/4 写入的完全一致；`project.yml` 的 YAML 键与 plist 的 XML 键同名同值。
