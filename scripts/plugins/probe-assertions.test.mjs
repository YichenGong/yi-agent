// 探针断言强度的结构性回归测试。
//
// 这些测试不执行攻击脚本（那是 WebView 里的 fixture），而是钉住「断言必须
// 存在失败路径」这件事。曾经 `alloc_burst_is_bounded` 的两条 return 都是
// `[true, ...]`——分配成功 return true，分配失败也 return true，于是这条判据
// 永远不可能红，标成 pass 等于什么都没验。这类恒真断言只有靠结构检查才挡得住：
// 功能测试永远看到 pass。

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, "..", "..");
const ATTACKS = path.join(
  REPO_ROOT,
  "plugins",
  "fixtures",
  "example-counter",
  "probe",
  "attacks.js",
);

/// 取出一个具名函数的源码体（从 `function <name>` 到下一个顶层 `}`）。
///
/// 刻意保持简单：attack 脚本是扁平的顶层函数集合，用一对大括号的深度计数
/// 足够，不必引入 JS 解析器。
function functionBody(source, name) {
  const marker = `function ${name}(`;
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `expected ${name} to exist in attacks.js`);
  let depth = 0;
  let index = source.indexOf("{", start);
  const bodyStart = index;
  for (; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    else if (source[index] === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(bodyStart, index + 1);
    }
  }
  assert.fail(`unbalanced braces in ${name}`);
}

test("alloc_burst_is_bounded can actually fail", () => {
  const source = readFileSync(ATTACKS, "utf8");
  const body = functionBody(source, "allocBurstIsBounded");
  assert.match(
    body,
    /return \[false,/,
    "the bounded-allocation probe must have a failing path; a probe whose every " +
      "return is [true, ...] cannot report anything and makes its pass meaningless",
  );
});

test("alloc_burst_is_bounded checks the host after the burst", () => {
  // 断言的关键在 post-condition：分配冲击之后必须确认页面仍与宿主通信。
  // 只断言「我分配了内存」是自证式通过。
  const source = readFileSync(ATTACKS, "utf8");
  const body = functionBody(source, "allocBurstIsBounded");
  assert.match(body, /hostState\s*\(/, "must re-contact the host after the allocation burst");
});

test("blocked recovery does not treat a page self-claim as evidence", () => {
  // 原生侧裁定逻辑在 Rust 里，这里钉住那条判据的意图不被悄悄改写：
  // 页面自报恢复必须是 FAIL 证据，而不是加分项。
  const rust = readFileSync(
    path.join(REPO_ROOT, "desktop", "src-tauri", "src", "plugin_host", "run.rs"),
    "utf8",
  );
  assert.match(
    rust,
    /page claimed recovery itself; that is not evidence/,
    "a page claiming recovery on its own must remain a failure condition",
  );
});
