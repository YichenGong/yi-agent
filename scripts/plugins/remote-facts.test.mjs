// 远端事实采集层的自测（计划 Task 1）。
//
// 这一层的全部职责是回答「远端到底是什么」，所以测试的重点是**它不许猜**：
// 未知架构不能默认成 aarch64，命令失败不能返回半份事实。猜错的后果是报告写
// 「架构匹配」而实际装错二进制，那个错误发生在远端且用户看不到。

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  isLoopbackHost,
  parseVersion,
  normalizeArch,
  collectRemoteFacts,
  SUPPORTED_ARCHES,
} from "./remote-facts.mjs";

test("every spelling of loopback is recognised", () => {
  // 整个 127.0.0.0/8 都是 loopback，不只是 127.0.0.1。只认最后一个等于给
  // 「127.0.0.2 冒充远端」留了门。
  for (const host of [
    "localhost",
    "LOCALHOST",
    "localhost.localdomain",
    "127.0.0.1",
    "127.0.0.2",
    "127.1.2.3",
    "127.255.255.255",
    "::1",
    "[::1]",
    "0.0.0.0",
  ]) {
    assert.equal(isLoopbackHost(host), true, `${host} must count as loopback`);
  }
});

test("a real host name is not mistaken for loopback", () => {
  for (const host of [
    "agenticlabsMac-mini-2.local",
    "10.103.132.15",
    "build.example.com",
    "1270.0.0.1",
  ]) {
    assert.equal(isLoopbackHost(host), false, `${host} must not count as loopback`);
  }
});

test("version is read from the real --version output", () => {
  assert.equal(parseVersion("yi-agent 0.1.13-dev+32365036\n"), "0.1.13-dev+32365036");
  assert.equal(parseVersion("  yi-agent   0.1.13  "), "0.1.13");
});

test("an unparseable version yields null instead of a guess", () => {
  // 猜一个版本会让「两端版本相等」这条判据失去意义。
  for (const text of ["", "garbage", "yi-agent", "yi-agent \n"]) {
    assert.equal(parseVersion(text), null, JSON.stringify(text));
  }
});

test("arm64 macOS normalises to the supported target triple", () => {
  assert.equal(normalizeArch("arm64", "Darwin"), "aarch64-apple-darwin");
  assert.equal(normalizeArch("x86_64", "Darwin"), "x86_64-apple-darwin");
  assert.equal(normalizeArch("x86_64", "Linux"), "x86_64-unknown-linux-gnu");
});

test("an unsupported architecture yields null, never a default", () => {
  // 默认成 aarch64 会让我们在一台 i686 机器上跑通「架构匹配」。
  for (const [machine, os] of [
    ["i686", "Linux"],
    ["armv7l", "Linux"],
    ["riscv64", "Linux"],
    ["", "Darwin"],
  ]) {
    assert.equal(normalizeArch(machine, os), null, `${machine}/${os}`);
  }
});

test("every normalised arch is one of the supported triples", () => {
  const produced = [
    normalizeArch("arm64", "Darwin"),
    normalizeArch("x86_64", "Darwin"),
    normalizeArch("x86_64", "Linux"),
  ];
  for (const arch of produced) {
    assert.ok(SUPPORTED_ARCHES.includes(arch), `${arch} must be supported`);
  }
});

test("facts are collected from four separate probes", () => {
  const seen = [];
  const run = (args) => {
    seen.push(args);
    const joined = args.join(" ");
    // 顺序要紧：`uname -s` 必须先于裸 `uname` 判断，否则会拿 arm64 冒充 OS 名。
    if (joined.includes("hostname")) return "agenticlabsMac-mini-2.local\n";
    if (joined.includes("sw_vers")) return "26.5.2\n";
    if (joined.includes("uname -s")) return "Darwin\n";
    if (joined.includes("uname -m")) return "arm64\n";
    if (joined.includes("--version")) return "yi-agent 0.1.13-dev+32365036\n";
    throw new Error(`unexpected probe: ${joined}`);
  };
  const facts = collectRemoteFacts({ run });
  assert.equal(facts.hostId, "agenticlabsMac-mini-2.local");
  assert.equal(facts.os, "macOS 26.5.2");
  assert.equal(facts.arch, "aarch64-apple-darwin");
  assert.equal(facts.version, "0.1.13-dev+32365036");
  assert.ok(seen.length >= 4, "each fact comes from its own probe");
});

test("a failing probe throws instead of returning half the facts", () => {
  // 半份事实会变成一份缺字段的证据，报告的读者无法判断缺的是哪一环。
  const run = (args) => {
    if (args.join(" ").includes("hostname")) throw new Error("ssh: connect failed");
    return "x\n";
  };
  assert.throws(() => collectRemoteFacts({ run }), /connect failed/);
});

test("a loopback host id is reported as such, not silently accepted", () => {
  const run = (args) => {
    const joined = args.join(" ");
    if (joined.includes("hostname")) return "localhost\n";
    if (joined.includes("sw_vers")) return "26.5.2\n";
    if (joined.includes("uname -m")) return "arm64\n";
    if (joined.includes("--version")) return "yi-agent 0.1.13\n";
    if (joined.includes("uname -s")) return "Darwin\n";
    throw new Error(`unexpected: ${joined}`);
  };
  const facts = collectRemoteFacts({ run });
  assert.equal(isLoopbackHost(facts.hostId), true);
});
