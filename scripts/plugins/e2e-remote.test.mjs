// 远端验收判定层的自测（计划 Task 3）。
//
// 判定层不连任何机器，全部注入。理由和上一层一样：需要真实网络的测试在门禁
// 里只会变成「本地绿 CI 红」或「CI 里被忽略」。
//
// 测试重心是**什么时候必须判失败**：版本不等、hostId 是 loopback、身份没变却
// 声称重连成功。这些反例才是一个验收执行器的价值所在。

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  parseArgs,
  judgeA16,
  judgeA17,
  isBindingCurrent,
  findLocalProcesses,
  UNPROVEN_A17,
} from "./e2e-remote.mjs";

const facts = {
  hostId: "agenticlabsMac-mini-2.local",
  os: "macOS 26.5.2",
  arch: "aarch64-apple-darwin",
  version: "0.1.13-dev+32365036",
};
const handshake = { protocolVersion: 1, sideHostInstanceId: "abc-123" };

test("A16 passes only when every check holds", () => {
  const result = judgeA16({
    facts,
    local: { version: facts.version },
    handshake,
    localProcesses: [],
    remotePath: "/r/yi-agent",
  });
  assert.equal(result.verdict, "pass");
  assert.deepEqual(result.remoteIdentity, {
    hostId: facts.hostId,
    os: facts.os,
    arch: facts.arch,
    version: facts.version,
  });
});

test("A16 fails when the versions differ, and says which check", () => {
  const result = judgeA16({
    facts,
    local: { version: "0.1.12" },
    handshake,
    localProcesses: [],
    remotePath: "/r/yi-agent",
  });
  assert.equal(result.verdict, "fail");
  const failing = result.checks.filter((check) => !check.ok).map((check) => check.name);
  assert.deepEqual(failing, ["version-exact"]);
});

test("A16 fails when the remote host id is loopback", () => {
  // 声明远端却给了 loopback，等于用一台机器冒充两台。
  const result = judgeA16({
    facts: { ...facts, hostId: "127.0.0.1" },
    local: { version: facts.version },
    handshake,
    localProcesses: [],
    remotePath: "/r/yi-agent",
  });
  assert.equal(result.verdict, "fail");
  assert.ok(result.checks.some((check) => check.name === "host-not-loopback" && !check.ok));
});

test("A16 fails when the handshake returned no instance id", () => {
  const result = judgeA16({
    facts,
    local: { version: facts.version },
    handshake: { protocolVersion: 1 },
    localProcesses: [],
    remotePath: "/r/yi-agent",
  });
  assert.equal(result.verdict, "fail");
  assert.ok(result.checks.some((check) => check.name === "host-instance-id" && !check.ok));
});

test("A16 fails when the architecture has no prebuilt target", () => {
  const result = judgeA16({
    facts: { ...facts, arch: null },
    local: { version: facts.version },
    handshake,
    localProcesses: [],
    remotePath: "/r/yi-agent",
  });
  assert.equal(result.verdict, "fail");
  assert.ok(result.checks.some((check) => check.name === "arch-supported" && !check.ok));
});

test("A16 fails when an unknown protocol version is answered", () => {
  // 协议版本不认识意味着两端说的不是同一套话，不该当成兼容。
  const result = judgeA16({
    facts,
    local: { version: facts.version },
    handshake: { protocolVersion: 2, sideHostInstanceId: "abc" },
    localProcesses: [],
    remotePath: "/r/yi-agent",
  });
  assert.equal(result.verdict, "fail");
  assert.ok(result.checks.some((check) => check.name === "protocol-version" && !check.ok));
});

test("A16 fails when a local process runs the remote program", () => {
  // 判据里写着「本地不运行远端程序」：本机出现该路径的进程就说明它被拉过来跑了。
  const result = judgeA16({
    facts,
    local: { version: facts.version },
    handshake,
    localProcesses: ["/r/yi-agent app-server --listen stdio://"],
    remotePath: "/r/yi-agent",
  });
  assert.equal(result.verdict, "fail");
  assert.ok(result.checks.some((check) => check.name === "no-local-remote-program" && !check.ok));
});

test("binding comparison uses the whole identity, not just the generation", () => {
  // 实测过：远端重连后 generation 仍是 1。只比代次会让重连看起来像同一台机器，
  // 而「重连不复用旧绑定」正是 A17 要证明的东西。
  const bound = { kind: "remote:p", instanceId: "a", generation: 1 };
  assert.equal(isBindingCurrent(bound, { kind: "remote:p", instanceId: "a", generation: 1 }), true);
  assert.equal(isBindingCurrent(bound, { kind: "remote:p", instanceId: "b", generation: 1 }), false);
  assert.equal(isBindingCurrent(bound, { kind: "remote:q", instanceId: "a", generation: 1 }), false);
  assert.equal(isBindingCurrent(bound, { kind: "remote:p", instanceId: "a", generation: 2 }), false);
  assert.equal(isBindingCurrent(null, { kind: "remote:p", instanceId: "a", generation: 1 }), false);
});

test("A17 reports a reconnect that reuses the old identity as a failure", () => {
  const result = judgeA17({
    first: { sideHostInstanceId: "same", generation: 1 },
    second: { sideHostInstanceId: "same", generation: 1 },
    orphanCount: 0,
    profileKind: "remote:probe",
  });
  assert.equal(result.verdict, "unsupported");
  assert.ok(result.checks.some((check) => check.name === "reconnect-new-identity" && !check.ok));
});

test("A17 sees a changed identity as proof even when the generation repeats", () => {
  const result = judgeA17({
    first: { sideHostInstanceId: "a", generation: 1 },
    second: { sideHostInstanceId: "b", generation: 1 },
    orphanCount: 0,
    profileKind: "remote:probe",
  });
  assert.equal(result.verdict, "unsupported");
  assert.ok(result.checks.some((check) => check.name === "reconnect-new-identity" && check.ok));
  // 显式记录代次相同：这正是「只比代次会误判」的证据。
  assert.ok(result.checks.some((check) => check.name === "generation-reused" && check.ok));
});

test("A17 reports leftover remote processes as a failure", () => {
  const result = judgeA17({
    first: { sideHostInstanceId: "a", generation: 1 },
    second: { sideHostInstanceId: "b", generation: 1 },
    orphanCount: 2,
    profileKind: "remote:probe",
  });
  assert.ok(result.checks.some((check) => check.name === "no-orphan" && !check.ok));
});

test("A17 never claims the whole case passed", () => {
  // 这条钉死 spec §4.2 的选择 (b)：A17 是一条判据而不是四条，只证明一部分就
  // 给 pass 会宣称整条成立。
  const result = judgeA17({
    first: { sideHostInstanceId: "a", generation: 1 },
    second: { sideHostInstanceId: "b", generation: 1 },
    orphanCount: 0,
    profileKind: "remote:probe",
  });
  assert.equal(result.verdict, "unsupported");
  assert.match(result.reason, /offline cache|asset/i);
  assert.ok(result.reason.includes(UNPROVEN_A17) || result.reason.length > 20);
  // 证据不能因为判词是 unsupported 就丢掉。
  assert.ok(result.remoteIdentity !== undefined);
});

test("parseArgs requires a host and an absolute remote path", () => {
  assert.deepEqual(
    parseArgs([
      "--case", "A16",
      "--host", "h",
      "--remote-path", "/r/y",
      "--local-version", "0.1.13",
      "--json",
    ]),
    {
      case: "A16",
      host: "h",
      remotePath: "/r/y",
      remoteHome: null,
      localVersion: "0.1.13",
      json: true,
    },
  );
  assert.throws(() => parseArgs(["--case", "A16", "--remote-path", "/r/y"]), /host/);
  assert.throws(
    () => parseArgs(["--case", "A16", "--host", "h", "--remote-path", "rel/y"]),
    /absolute/,
  );
  assert.throws(
    () => parseArgs(["--case", "A99", "--host", "h", "--remote-path", "/r/y"]),
    /case/,
  );
});

test("A16 refuses to compare versions when it does not know the local one", () => {
  // 没有本机版本就无法判断「相等」。默认成相等会让这条判据永远成立。
  assert.throws(
    () => parseArgs(["--case", "A16", "--host", "h", "--remote-path", "/r/y"]),
    /local-version/,
  );
});

test("the host and path fall back to the acceptance environment variables", () => {
  // e2e.mjs 的 parseCommand 不过 shell，命令里写 $VAR 只会得到字面量。所以
  // 映射表里的命令不带参数，靠环境变量传——这条钉住那条通路。
  const args = parseArgs(["--case", "A16", "--json"], {
    env: {
      YI_AGENT_ACCEPTANCE_REMOTE_HOST: "agenticlabs@10.103.132.15",
      YI_AGENT_ACCEPTANCE_REMOTE_PATH: "/r/yi-agent",
      YI_AGENT_ACCEPTANCE_LOCAL_VERSION: "0.1.13",
    },
  });
  assert.equal(args.host, "agenticlabs@10.103.132.15");
  assert.equal(args.remotePath, "/r/yi-agent");
  assert.equal(args.localVersion, "0.1.13");
});

test("an empty host environment variable still counts as no host", () => {
  // 空字符串不能被当成「有主机」，否则会去连一个空地址。
  assert.throws(
    () => parseArgs(["--case", "A17", "--json"], { env: { YI_AGENT_ACCEPTANCE_REMOTE_HOST: "  " } }),
    /host/,
  );
});

test("flags beat the environment", () => {
  const args = parseArgs(
    ["--case", "A16", "--host", "explicit", "--remote-path", "/r/y", "--local-version", "9"],
    { env: { YI_AGENT_ACCEPTANCE_REMOTE_HOST: "from-env" } },
  );
  assert.equal(args.host, "explicit");
});

test("the local process check does not match the checking shell", () => {
  // `ps axo comm=` 给的是内核记录的可执行文件。调用链上的 sh/node 都只是
  // `/bin/sh`、`node`，不会因为参数里提到 remotePath 就被算成执行它。
  const ps = [
    "  123 node",
    "  124 /bin/sh",
    "  125 /bin/zsh",
  ].join("\n");
  assert.deepEqual(findLocalProcesses("/r/yi-agent", { psOutput: ps, selfPid: 123 }), []);
});

test("a process whose executable is the remote binary is caught", () => {
  const ps = [
    "  123 node",
    "  456 /r/yi-agent",
  ].join("\n");
  const found = findLocalProcesses("/r/yi-agent", { psOutput: ps, selfPid: 123 });
  assert.deepEqual(found, ["/r/yi-agent"]);
});

test("a shell wrapper that really execs the remote binary is caught", () => {
  // `sh -c '/r/yi-agent app-server ...'` 的 comm 是 `/bin/sh`，但第一个非
  // 解释器词就是那个二进制——这是真的在运行它。
  const ps = "  456 /bin/sh -c /r/yi-agent app-server";
  const found = findLocalProcesses("/r/yi-agent", { psOutput: ps, selfPid: 1 });
  assert.equal(found.length, 1);
});

test("a path that merely shares a prefix is not a match", () => {
  assert.deepEqual(findLocalProcesses("/r/yi-agent", { psOutput: "  456 /r/yi-agent.bak", selfPid: 1 }), []);
});

test("a path appearing only as a later argument is not an execution", () => {
  const ps = "  456 node /some/script.mjs --remote-path /r/yi-agent";
  assert.deepEqual(findLocalProcesses("/r/yi-agent", { psOutput: ps, selfPid: 1 }), []);
});

test("A17 keeps a real remote identity even though its verdict is unsupported", () => {
  // 判词不是 pass 不等于证据可以空着：缺身份的 remote 证据会被校验器拒绝，
  // 而读者也需要知道这是在哪个远端上验的。
  const result = judgeA17({
    first: { sideHostInstanceId: "a", generation: 1 },
    second: { sideHostInstanceId: "b", generation: 1 },
    orphanCount: 0,
    profileKind: "remote:probe",
    facts: {
      hostId: "agenticlabsMac-mini-2.local",
      os: "macOS 26.5.2",
      arch: "aarch64-apple-darwin",
      version: "0.1.13",
    },
  });
  assert.deepEqual(result.remoteIdentity, {
    hostId: "agenticlabsMac-mini-2.local",
    os: "macOS 26.5.2",
    arch: "aarch64-apple-darwin",
    version: "0.1.13",
  });
});
