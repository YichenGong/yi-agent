import { describe, expect, it } from "vitest";
import { listMcpServers, type McpRpc } from "./mcp";

const rpc = (result: unknown): McpRpc => (async () => result) as unknown as McpRpc;

describe("listMcpServers", () => {
  it("sends the project when one is given", async () => {
    const calls: unknown[] = [];
    const call = (async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { configured: false, path: "/p/.yi-agent/mcp.json", enabled: false, servers: [] };
    }) as unknown as McpRpc;
    await listMcpServers(call, "/p");
    expect(calls).toEqual([["mcp/list", { project: "/p" }]]);
  });

  it("omits the project when none is given", async () => {
    const calls: unknown[] = [];
    const call = (async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { configured: false, path: "/p/.yi-agent/mcp.json", enabled: false, servers: [] };
    }) as unknown as McpRpc;
    await listMcpServers(call);
    expect(calls).toEqual([["mcp/list", {}]]);
  });

  it("keeps unprobed tools as null instead of an empty list", async () => {
    const out = await listMcpServers(
      rpc({
        configured: true,
        path: "/p/.yi-agent/mcp.json",
        enabled: true,
        servers: [
          {
            name: "fs",
            kind: "stdio",
            enabled: true,
            command: "npx",
            args: ["-y"],
            tools: null,
            tool_count: 0,
            read_only_count: 0,
          },
        ],
      }),
    );
    expect(out.servers[0].tools).toBeNull();
    expect(out.servers[0].command).toBe("npx");
  });

  it("drops rows with an unknown transport instead of guessing", async () => {
    const out = await listMcpServers(
      rpc({
        configured: true,
        path: "/p/.yi-agent/mcp.json",
        enabled: true,
        servers: [
          { name: "bad", kind: "carrier-pigeon" },
          { name: "fs", kind: "stdio", command: "npx" },
        ],
      }),
    );
    expect(out.servers.map((s) => s.name)).toEqual(["fs"]);
  });

  it("fills missing fields with safe empties", async () => {
    const out = await listMcpServers(rpc({ servers: [{ name: "fs", kind: "stdio" }] }));
    expect(out.configured).toBe(false);
    expect(out.servers[0]).toMatchObject({
      enabled: true,
      args: [],
      envKeys: [],
      headerKeys: [],
      tools: null,
      toolCount: 0,
      readOnlyCount: 0,
    });
  });
});