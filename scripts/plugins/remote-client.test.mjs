// ssh stdio JSON-RPC 客户端的自测（计划 Task 2）。
//
// 全部注入 fake spawn，不连真机：门禁机器上没有远端主机，需要真实网络的测试
// 只会变成「本地绿、CI 红」或者「本地红、被当环境抖动」——两种都让人学会忽略它。
//
// 这里钉的是**失败时的行为**：超时抛错而不是返回 null，子进程提前退出不挂死，
// close 幂等。连上机器是容易的，不挂在机器上才难。

import assert from "node:assert/strict";
import { test } from "node:test";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

import { buildSshArgs, RemoteSession } from "./remote-client.mjs";

/// 造一个假 ssh 子进程：stdin 收到的行交给 onLine，onLine 返回要回写的行。
function fakeSpawn({ onLine, exitAfter } = {}) {
  const child = new EventEmitter();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  const stdinLines = [];
  let buffer = "";
  child.stdin = new PassThrough();
  child.stdin.on("data", (chunk) => {
    buffer += chunk.toString();
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      stdinLines.push(line);
      if (exitAfter !== undefined) {
        exitAfter -= 1;
        if (exitAfter <= 0) {
          setImmediate(() => child.emit("exit", 1));
          continue;
        }
      }
      const reply = onLine?.(line);
      if (reply) setImmediate(() => child.stdout.write(reply + "\n"));
    }
  });
  child.kill = () => {
    child.emit("exit", null);
  };
  child.stdinLines = stdinLines;
  return child;
}

test("ssh args quote a remote path containing spaces", () => {
  // 未引用时 `/Users/a b/yi-agent` 会被远端 sh 拆成两个词，报一个与真实原因
  // 无关的错误。
  const args = buildSshArgs({
    host: "user@10.0.0.5",
    remotePath: "/Users/a b/yi-agent",
    remoteHome: "/tmp/home",
  });
  const joined = args.join(" ");
  assert.ok(joined.includes("'/Users/a b/yi-agent'"), joined);
});

test("ssh args pass YI_AGENT_HOME on the remote command line", () => {
  const args = buildSshArgs({
    host: "h",
    remotePath: "/r/yi-agent",
    remoteHome: "/tmp/h",
  });
  const joined = args.join(" ");
  assert.ok(joined.includes("YI_AGENT_HOME=/tmp/h"), joined);
  assert.ok(joined.includes("app-server --listen stdio://"), joined);
});

test("a normal handshake returns the host instance id", async () => {
  const child = fakeSpawn({
    onLine: (line) => {
      const msg = JSON.parse(line);
      if (msg.method === "initialize") {
        return JSON.stringify({
          jsonrpc: "2.0",
          id: msg.id,
          result: {
            protocolVersion: 1,
            serverInfo: { name: "yi-agent-app-server", version: "0.1.13" },
            sideHostInstanceId: "abc-123",
          },
        });
      }
      return null;
    },
  });
  const session = await RemoteSession.connect({
    host: "h",
    remotePath: "/r/yi-agent",
    spawn: () => child,
    timeoutMs: 200,
  });
  const hello = await session.initialize();
  assert.equal(hello.sideHostInstanceId, "abc-123");
  assert.equal(hello.protocolVersion, 1);
  session.close();
});

test("a handshake that never answers times out instead of hanging", async () => {
  const child = fakeSpawn({ onLine: () => null });
  const session = await RemoteSession.connect({
    host: "h",
    remotePath: "/r/yi-agent",
    spawn: () => child,
    timeoutMs: 120,
  });
  await assert.rejects(() => session.initialize(), /timed out/i);
  session.close();
});

test("a child that exits early rejects instead of waiting forever", async () => {
  const child = fakeSpawn({ onLine: () => null, exitAfter: 1 });
  const session = await RemoteSession.connect({
    host: "h",
    remotePath: "/r/yi-agent",
    spawn: () => child,
    timeoutMs: 500,
  });
  await assert.rejects(() => session.initialize(), /exited|exit/i);
  session.close();
});

test("close is idempotent", async () => {
  const child = fakeSpawn({ onLine: () => null });
  const session = await RemoteSession.connect({
    host: "h",
    remotePath: "/r/yi-agent",
    spawn: () => child,
    timeoutMs: 200,
  });
  session.close();
  session.close();
});

test("a reply with a different id is not matched to this request", async () => {
  // 握手期间可能有通知或别的响应先到。按「第一行就是答案」实现会把它当成
  // initialize 的结果，然后读到 undefined 的 sideHostInstanceId。
  const child = fakeSpawn({
    onLine: (line) => {
      const msg = JSON.parse(line);
      if (msg.method !== "initialize") return null;
      return [
        JSON.stringify({ jsonrpc: "2.0", method: "notice", params: {} }),
        JSON.stringify({ jsonrpc: "2.0", id: 999, result: { wrong: true } }),
        JSON.stringify({
          jsonrpc: "2.0",
          id: msg.id,
          result: { protocolVersion: 1, sideHostInstanceId: "right-one" },
        }),
      ].join("\n");
    },
  });
  const session = await RemoteSession.connect({
    host: "h",
    remotePath: "/r/yi-agent",
    spawn: () => child,
    timeoutMs: 300,
  });
  const hello = await session.initialize();
  assert.equal(hello.sideHostInstanceId, "right-one");
  session.close();
});

test("an initialize result without an instance id is rejected", async () => {
  // 缺字段时返回 undefined 会一路变成证据里的空 hostId，那比直接报错更难查。
  const child = fakeSpawn({
    onLine: (line) => {
      const msg = JSON.parse(line);
      if (msg.method !== "initialize") return null;
      return JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { protocolVersion: 1 } });
    },
  });
  const session = await RemoteSession.connect({
    host: "h",
    remotePath: "/r/yi-agent",
    spawn: () => child,
    timeoutMs: 300,
  });
  await assert.rejects(() => session.initialize(), /sideHostInstanceId/);
  session.close();
});
