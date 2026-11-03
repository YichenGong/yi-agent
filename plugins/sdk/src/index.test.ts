import { describe, expect, it, vi } from "vitest";

import {
  BRIDGE_API_VERSION,
  PluginBridgeError,
  connect,
  connectOrNull,
  type HostHello,
  type MessagePortLike,
  type PluginContext,
} from "./index";

/**
 * 这些测试用一个真实的消息泵模拟宿主：`FakePort` 在收到页面消息后，像宿主那样
 * 回一条 `hello` 或 `response`。不 mock SDK 自己的协议解析——被测的正是那段。
 */

const context: PluginContext = {
  api: "v1",
  plugin: { id: "example-counter", version: "0.1.0" },
  host: { id: "host-local", version: "1.0.0" },
  project: { handle: "ph1_aaaa" },
  activation: 1,
  theme: "light",
};

type HostBehaviour = (message: Record<string, unknown>, reply: (data: unknown) => void) => void;

class FakePort implements MessagePortLike {
  readonly sent: Record<string, unknown>[] = [];
  private listener: ((event: { data: unknown }) => void) | null = null;

  constructor(private readonly host: HostBehaviour) {}

  postMessage(message: unknown): void {
    const record = message as Record<string, unknown>;
    this.sent.push(record);
    // 异步回复，模拟跨端口投递。
    queueMicrotask(() => this.host(record, (data) => this.deliver(data)));
  }

  addEventListener(_type: "message", listener: (event: { data: unknown }) => void): void {
    this.listener = listener;
  }

  removeEventListener(): void {
    this.listener = null;
  }

  start(): void {}

  deliver(data: unknown): void {
    this.listener?.({ data });
  }
}

function helloHost(overrides: Partial<HostHello> = {}): HostBehaviour {
  return (message, reply) => {
    if (message.type !== "init") return;
    reply({
      type: "hello",
      api: "v1",
      context,
      token: "tok-1",
      ...overrides,
    } satisfies HostHello);
  };
}

describe("handshake", () => {
  it("sends an init message and adopts the host context", async () => {
    const port = new FakePort(helloHost());
    const api = await connect({ port, nonce: "n-1", helloTimeoutMs: 500 });

    expect(port.sent[0]).toMatchObject({ type: "init", nonce: "n-1", uiApi: "v1" });
    await expect(api.context()).resolves.toEqual(context);
    expect(BRIDGE_API_VERSION).toBe("v1");
  });

  it("fails with Incompatible when the host speaks another api version", async () => {
    const port = new FakePort(helloHost({ api: "v2" }));
    await expect(connect({ port, nonce: "n", helloTimeoutMs: 500 })).rejects.toMatchObject({
      kind: "Incompatible",
    });
  });

  it("fails when no port was injected at all", async () => {
    delete (globalThis as Record<string, unknown>).__yiAgentPluginHost;
    await expect(connect()).rejects.toBeInstanceOf(PluginBridgeError);
    await expect(connect()).rejects.toMatchObject({ kind: "PermissionDenied" });
  });

  it("connectOrNull hides the failure from render paths", async () => {
    delete (globalThis as Record<string, unknown>).__yiAgentPluginHost;
    await expect(connectOrNull()).resolves.toBeNull();
  });

  it("times out when the host never greets", async () => {
    const port = new FakePort(() => {});
    await expect(connect({ port, nonce: "n", helloTimeoutMs: 20 })).rejects.toMatchObject({
      kind: "Io",
    });
  });
});

describe("request", () => {
  function requestHost(result: unknown): HostBehaviour {
    return (message, reply) => {
      if (message.type === "init") {
        reply({ type: "hello", api: "v1", context, token: "tok" });
        return;
      }
      if (message.type === "request") {
        reply({ type: "response", requestId: message.requestId, ok: true, result });
      }
    };
  }

  it("carries the token and resolves with the host result", async () => {
    const port = new FakePort(requestHost({ total: 3 }));
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });
    await expect(api.request("counter/read", {})).resolves.toEqual({ total: 3 });

    const request = port.sent.find((message) => message.type === "request");
    expect(request).toMatchObject({ method: "counter/read", params: {}, token: "tok" });
  });

  it("surfaces a classified host error", async () => {
    const port = new FakePort((message, reply) => {
      if (message.type === "init") {
        reply({ type: "hello", api: "v1", context, token: "tok" });
        return;
      }
      reply({
        type: "response",
        requestId: message.requestId,
        ok: false,
        error: { kind: "Conflict", detail: "request id reused" },
      });
    });
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });

    const failure = await api.request("counter/increment", { by: 1 }).catch((error) => error);
    expect(failure).toBeInstanceOf(PluginBridgeError);
    expect(failure.kind).toBe("Conflict");
    expect(failure.detail).toBe("request id reused");
  });

  it("mints a distinct request id per call", async () => {
    const port = new FakePort(requestHost(null));
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });
    await api.request("counter/read", {});
    await api.request("counter/read", {});
    const ids = port.sent.filter((m) => m.type === "request").map((m) => m.requestId);
    expect(new Set(ids).size).toBe(2);
  });

  it("queries an unknown result through the operationResult method", async () => {
    const port = new FakePort(requestHost({ status: "unknown", requestId: "r-9", result: null }));
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });
    const result = await api.result("r-9");
    expect(result.status).toBe("unknown");

    const last = port.sent.filter((m) => m.type === "request").at(-1);
    expect(last).toMatchObject({
      method: "plugins/v1/operationResult",
      params: { requestId: "r-9" },
    });
  });
});

describe("subscribe", () => {
  it("delivers events and stops after unsubscribe", async () => {
    let hostReply!: (data: unknown) => void;
    const port = new FakePort((message, reply) => {
      if (message.type === "init") {
        reply({ type: "hello", api: "v1", context, token: "tok" });
        return;
      }
      if (message.type === "request" && message.method === "subscribe") {
        hostReply = reply;
        reply({ type: "response", requestId: message.requestId, ok: true, result: null });
      }
    });
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });

    const seen: number[] = [];
    const stop = await api.subscribe<{ total: number }>("counter.state", (event) => {
      seen.push(event.payload.total);
    });

    port.deliver({ type: "event", topic: "counter.state", revision: 1, payload: { total: 1 } });
    port.deliver({ type: "event", topic: "counter.state", revision: 2, payload: { total: 2 } });
    stop();
    port.deliver({ type: "event", topic: "counter.state", revision: 3, payload: { total: 3 } });

    expect(seen).toEqual([1, 2]);
    expect(hostReply).toBeTruthy();
    expect(port.sent.at(-1)).toMatchObject({ type: "unsubscribe", topic: "counter.state" });
  });

  it("replays a snapshot through the same handler", async () => {
    const port = new FakePort((message, reply) => {
      if (message.type === "init") {
        reply({ type: "hello", api: "v1", context, token: "tok" });
        return;
      }
      reply({
        type: "response",
        requestId: message.requestId,
        ok: true,
        result: { topic: "counter.state", revision: 7, state: { total: 7 } },
      });
    });
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });

    const seen: number[] = [];
    await api.subscribe<{ total: number }>("counter.state", (event) => seen.push(event.payload.total));
    expect(seen).toEqual([7]);
  });

  it("refuses a duplicate subscription to the same topic", async () => {
    const port = new FakePort((message, reply) => {
      if (message.type === "init") {
        reply({ type: "hello", api: "v1", context, token: "tok" });
        return;
      }
      reply({ type: "response", requestId: message.requestId, ok: true, result: null });
    });
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });
    await api.subscribe("counter.state", () => {});
    await expect(api.subscribe("counter.state", () => {})).rejects.toMatchObject({
      kind: "Conflict",
    });
  });

  it("rolls the local subscription back when the host refuses", async () => {
    const port = new FakePort((message, reply) => {
      if (message.type === "init") {
        reply({ type: "hello", api: "v1", context, token: "tok" });
        return;
      }
      reply({
        type: "response",
        requestId: message.requestId,
        ok: false,
        error: { kind: "RecoveryRequired", detail: "no snapshot" },
      });
    });
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });
    await expect(api.subscribe("counter.state", () => {})).rejects.toMatchObject({
      kind: "RecoveryRequired",
    });
    // 回滚后可以再次尝试，不会被误判成重复订阅。
    await expect(api.subscribe("counter.state", () => {})).rejects.toMatchObject({
      kind: "RecoveryRequired",
    });
  });
});

describe("presentation methods", () => {
  it("routes openThread, notify and confirm through the bridge", async () => {
    const calls: Record<string, unknown>[] = [];
    const port = new FakePort((message, reply) => {
      if (message.type === "init") {
        reply({ type: "hello", api: "v1", context, token: "tok" });
        return;
      }
      calls.push(message);
      const result = message.method === "confirm" ? true : null;
      reply({ type: "response", requestId: message.requestId, ok: true, result });
    });
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });

    await api.openThread("thread-1");
    await api.notify("warning", "careful");
    await expect(api.confirm({ title: "Delete?", body: "This cannot be undone", destructive: true })).resolves.toBe(true);

    expect(calls.map((call) => call.method)).toEqual(["openThread", "notify", "confirm"]);
    expect(calls[0].params).toEqual({ threadId: "thread-1" });
    expect(calls[1].params).toEqual({ level: "warning", message: "careful" });
  });
});

describe("isolation from the host", () => {
  it("has no Tauri dependency and no host source path in the built output", async () => {
    const fs = await import("node:fs/promises");
    const path = await import("node:path");

    const dist = path.join(process.cwd(), "dist");
    const files = await fs.readdir(dist).catch(() => [] as string[]);
    expect(files.length).toBeGreaterThan(0);

    const js = files.filter((file) => file.endsWith(".js"));
    for (const file of js) {
      const text = await fs.readFile(path.join(dist, file), "utf8");
      expect(text).not.toContain("@tauri-apps");
      expect(text).not.toContain("desktop/src");
      expect(text).not.toContain("pluginHost/");
      expect(text).not.toContain("invoke(");
    }
  });

  it("declares no runtime dependency on the desktop package", async () => {
    const fs = await import("node:fs/promises");
    const path = await import("node:path");
    const manifest = JSON.parse(
      await fs.readFile(path.join(process.cwd(), "package.json"), "utf8"),
    ) as Record<string, unknown>;
    expect(manifest.dependencies ?? {}).toEqual({});
    expect(fs).toBeTruthy();
  });
});

describe("no identity parameters exist", () => {
  it("never lets a caller claim a plugin or project", async () => {
    const port = new FakePort(helloHost());
    const api = await connect({ port, nonce: "n", helloTimeoutMs: 500 });
    await api.context();

    const init = port.sent[0];
    expect(Object.keys(init).sort()).toEqual(["nonce", "type", "uiApi"]);
    // 报文里不出现插件 / 项目 / 代次，它们由宿主按 frame 决定。
    expect(JSON.stringify(init)).not.toContain("plugin");
    expect(JSON.stringify(init)).not.toContain("ph1_");
    expect(vi.isMockFunction(api.request)).toBe(false);
  });
});
