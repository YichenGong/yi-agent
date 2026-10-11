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
