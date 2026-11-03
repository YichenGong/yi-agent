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

/**
 * iOS 版本号的静态门禁（前端侧）。
 *
 * 与 `scripts/test_ios_version_sync.sh`（CI 的 `ios-version-check`）是两道不同的
 * 门：那边验的是「注入脚本能正确写」+「提交进仓库的值等于主线版本」，这里验的是
 * 「真实产物文件里确实是主线版本」——即 CI 之外，跑一次前端测试也能发现漂移。
 *
 * 真源是 `yi-agent-rs/Cargo.toml` 的 `[workspace.package] version`，三处 iOS
 * 版本都必须等于它。此前它们一起停在模板默认的 0.1.0，而主线早已 0.1.13：
 * 装到手机上的包自报的版本与主线无关，用户无从判断该不该升级。
 */
const cargoToml = () =>
  readFileSync(`${dir}/../../yi-agent-rs/Cargo.toml`, "utf8");

const workspaceVersion = (): string => {
  // [workspace.package] 下的第一处 version = "x.y.z"
  const m = cargoToml().match(/^version = "([^"]+)"$/m);
  if (!m) throw new Error("cannot read workspace version from Cargo.toml");
  return m[1];
};

describe("iOS version follows the workspace version", () => {
  it("reads a semver version from Cargo.toml", () => {
    // CFBundleShortVersionString 只接受三段整数：带 prerelease 时 Tauri 会静默
    // 截断，产物与主线悄悄分叉。所以真源本身也必须是这个形状。
    expect(workspaceVersion()).toMatch(/^\d+\.\d+\.\d+$/);
  });

  it("uses the workspace version in the built Info.plist", () => {
    const v = workspaceVersion();
    const plist = infoPlist();
    expect(plist).toMatch(
      new RegExp(
        `<key>CFBundleShortVersionString</key>\\s*<string>${v}</string>`,
      ),
    );
    expect(plist).toMatch(
      new RegExp(`<key>CFBundleVersion</key>\\s*<string>${v}</string>`),
    );
  });

  it("uses the workspace version in project.yml (the init source)", () => {
    const v = workspaceVersion();
    const yml = projectYml();
    expect(yml).toMatch(
      new RegExp(`^\\s*CFBundleShortVersionString:\\s*"?${v}"?\\s*$`, "m"),
    );
    expect(yml).toMatch(new RegExp(`^\\s*CFBundleVersion:\\s*"?${v}"?\\s*$`, "m"));
  });

  it("uses the workspace version in tauri.conf.json", () => {
    const v = workspaceVersion();
    const conf = readFileSync(`${dir}/../src-tauri/tauri.conf.json`, "utf8");
    expect(conf).toMatch(new RegExp(`"version":\\s*"${v}"`));
  });
});
