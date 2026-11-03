// Behavioral tests for the Magpie agent plugin.
//
// `connect()` runs inside Magpie's moejs runtime (no Node APIs), so the plugin
// itself is a pure mapping. These tests exercise that mapping directly, plus a
// small simulation of Magpie's per-key write protocol (backup -> write ->
// restore) to cover the "connect / switch / disconnect restores the user's
// config" behavior without requiring a running Magpie.
//
// Run with: node --test tests/agent.test.mjs   (from integrations/magpie-agent-yi-agent/)

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync, rmSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { agent, connect, _internal } from "../yi-agent.agent.js";

// ---- pure mapping ---------------------------------------------------------

test("agent export carries the icon Magpie's Agents page draws", () => {
  // Magpie renders the Agents row from `agent.icon` (agentplug Desc.Icon); it
  // does not fall back to package.json's `magpie.icon`, which only feeds the
  // Installed list. Without this field the row shows the generic placeholder.
  assert.equal(typeof agent.icon, "string");
  assert.match(agent.icon, /^data:image\/png;base64,/);

  // Same picture as the one the Installed list uses, so both rows match.
  const pkg = JSON.parse(
    readFileSync(join(dirname(fileURLToPath(import.meta.url)), "..", "package.json"), "utf8"),
  );
  assert.equal(agent.icon, pkg.magpie.icon);

  // Decodes to a real PNG (signature + IHDR), not a truncated string.
  const png = Buffer.from(agent.icon.split(",")[1], "base64");
  assert.deepEqual(png.subarray(0, 8), Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
  assert.equal(png.subarray(12, 16).toString("latin1"), "IHDR");
});

test("agent declaration uses the models.json convention", () => {
  assert.equal(agent.id, "yi-agent");
  assert.equal(agent.config, "~/.yi-agent/models.json");
  assert.equal(agent.model, "default_model");
  assert.equal(agent.prefix, _internal.PREFIX);
});

test("toEntry maps a Magpie model to an openai gateway entry", () => {
  const entry = _internal.toEntry(
    { id: "anthropic/claude-sonnet-4-5", context: 200000 },
    { v1: "http://127.0.0.1:3425/v1", key: "yi-agent-key" },
  );
  assert.equal(entry.name, "magpie/anthropic/claude-sonnet-4-5");
  assert.equal(entry.provider, "openai");
  assert.equal(entry.api_url, "http://127.0.0.1:3425/v1");
  assert.equal(entry.api_key, "yi-agent-key");
  assert.equal(entry.model, "anthropic/claude-sonnet-4-5");
  assert.equal(entry.context_length, 200000);
});

test("toEntry omits context_length when absent or unusable", () => {
  const gateway = { v1: "g/v1", key: "k" };
  const cases = [
    { id: "a" }, // absent
    { id: "a", context: undefined },
    { id: "a", context: null },
    { id: "a", context: "200000" }, // string, not number
    { id: "a", context: 0 }, // zero is "undeclared" in Yi Agent
    { id: "a", context: -1 }, // negative
    { id: "a", context: 4294967296 }, // > u32::MAX
    { id: "a", context: 1.5 }, // not an integer
  ];
  for (const model of cases) {
    const entry = _internal.toEntry(model, gateway);
    assert.equal(
      entry.context_length,
      undefined,
      `context_length must be omitted for ${JSON.stringify(model)}`,
    );
  }
});

test("toEntry accepts u32::MAX as a valid context length", () => {
  const entry = _internal.toEntry(
    { id: "a", context: 4294967295 },
    { v1: "g/v1", key: "k" },
  );
  assert.equal(entry.context_length, 4294967295);
});

test("connect maps every model and syncs subagent_model to the selection", () => {
  const input = {
    gateway: { v1: "http://127.0.0.1:3425/v1", key: "yi-agent-key" },
    model: { id: "anthropic/claude-sonnet-4-5", context: 200000 },
    models: [
      { id: "anthropic/claude-sonnet-4-5", context: 200000 },
      { id: "openai/gpt-4o", context: 128000 },
    ],
  };
  const out = connect(input);
  assert.equal(out.models.length, 2);
  assert.equal(out.models[0].name, "magpie/anthropic/claude-sonnet-4-5");
  assert.equal(out.models[1].name, "magpie/openai/gpt-4o");
  assert.equal(out.subagent_model, "magpie/anthropic/claude-sonnet-4-5");
});

test("connect leaves subagent_model unset when there is no selection", () => {
  const input = {
    gateway: { v1: "g/v1", key: "k" },
    model: undefined,
    models: [{ id: "a", context: 100 }],
  };
  const out = connect(input);
  assert.equal(out.subagent_model, undefined);
});

test("default_model Magpie writes equals the entry name it emits", () => {
  // Magpie writes default_model = agent.prefix + model.id. The entry's `name`
  // must equal that exact string, or Yi Agent can't resolve it.
  const id = "anthropic/claude-sonnet-4-5";
  const input = {
    gateway: { v1: "g/v1", key: "k" },
    model: { id },
    models: [{ id, context: 100 }],
  };
  const out = connect(input);
  const writtenDefault = agent.prefix + id;
  const names = out.models.map((m) => m.name);
  assert.ok(names.includes(writtenDefault), `no entry named ${writtenDefault}`);
});

test("connect skips models without an id", () => {
  const input = {
    gateway: { v1: "g/v1", key: "k" },
    model: undefined,
    models: [{ context: 100 }, null, undefined, "not-an-object"],
  };
  const out = connect(input);
  assert.equal(out.models.length, 0);
});

test("connect is deterministic across repeated calls with the same directory", () => {
  const input = {
    gateway: { v1: "g/v1", key: "k" },
    model: { id: "a", context: 100 },
    models: [{ id: "a", context: 100 }, { id: "b", context: 200 }],
  };
  assert.deepEqual(connect(input), connect(input));
});

// ---- Magpie write-protocol simulation (backup -> write -> restore) ---------
//
// This mirrors Magpie's documented behavior for agents: each key it manages is
// backed up (typed), set in place, and restored on the way back, with keys that
// weren't there removed. The keys this plugin manages are `models` and
// `subagent_model`; Magpie itself manages `default_model` (agent.model).

function loadModels(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return {}; // no config file
  }
}

// The exact key set Magpie manages for this agent.
const MANAGED_KEYS = ["models", "default_model", "subagent_model"];

function snapshotKeys(obj, keys) {
  const out = {};
  for (const k of keys) {
    if (Object.prototype.hasOwnProperty.call(obj, k)) {
      out[k] = obj[k];
    }
  }
  return out;
}

// Magpie merges connect()'s output plus default_model = prefix + model.id into
// the user's file, keeping unrelated top-level keys untouched.
function writeConnected(path, connectOutput, writtenDefault) {
  const original = loadModels(path);
  const merged = { ...original };
  for (const k of MANAGED_KEYS) {
    delete merged[k];
  }
  merged.models = connectOutput.models;
  merged.default_model = writtenDefault;
  if (connectOutput.subagent_model !== undefined) {
    merged.subagent_model = connectOutput.subagent_model;
  }
  writeFileSync(path, JSON.stringify(merged, null, 2));
}

// Magpie restores: original values come back, keys that weren't there disappear.
function restore(path, backup) {
  const current = loadModels(path);
  const restored = { ...current };
  for (const k of MANAGED_KEYS) {
    delete restored[k];
  }
  for (const k of Object.keys(backup)) {
    restored[k] = backup[k];
  }
  writeFileSync(path, JSON.stringify(restored, null, 2));
}

function tempModelsPath() {
  // macOS TMPDIR (/var/folders/...) can be sandboxed with EPERM for the test
  // runner, so isolate under this plugin's own directory instead.
  const here = dirname(fileURLToPath(import.meta.url));
  const root = join(here, ".tmp");
  mkdirSync(root, { recursive: true });
  return join(mkdtempSync(join(root, "models-")), "models.json");
}

test("no existing config file: connect creates it, disconnect removes it", () => {
  const path = tempModelsPath();
  const original = loadModels(path);
  assert.deepEqual(original, {});

  const input = {
    gateway: { v1: "g/v1", key: "k" },
    model: { id: "a", context: 100 },
    models: [{ id: "a", context: 100 }],
  };
  const out = connect(input);
  const backup = snapshotKeys(original, MANAGED_KEYS);
  writeConnected(path, out, "magpie/a");

  const connected = loadModels(path);
  assert.equal(connected.default_model, "magpie/a");
  assert.equal(connected.subagent_model, "magpie/a");
  assert.equal(connected.models.length, 1);

  restore(path, backup);
  assert.deepEqual(loadModels(path), {}, "disconnect must remove every managed key");
  rmSync(path);
});

test("existing config with unrelated keys and a subagent_model is restored", () => {
  const path = tempModelsPath();
  writeFileSync(
    path,
    JSON.stringify({
      models: [{ name: "mine", provider: "openai", api_url: "u", model: "m", api_key: "k" }],
      default_model: "mine",
      subagent_model: "mine",
      future_key: 42,
    }),
  );
  const original = loadModels(path);
  const backup = snapshotKeys(original, MANAGED_KEYS);

  const input = {
    gateway: { v1: "g/v1", key: "k" },
    model: { id: "a", context: 100 },
    models: [{ id: "a", context: 100 }],
  };
  writeConnected(path, connect(input), "magpie/a");

  // The original entry is out of the array while connected.
  const connected = loadModels(path);
  assert.equal(connected.models.length, 1);
  assert.equal(connected.models[0].name, "magpie/a");

  restore(path, backup);
  const restored = loadModels(path);
  assert.equal(restored.models.length, 1);
  assert.equal(restored.models[0].name, "mine");
  assert.equal(restored.default_model, "mine");
  assert.equal(restored.subagent_model, "mine");
  assert.equal(restored.future_key, 42, "unrelated top-level keys survive");
  rmSync(path);
});

test("subagent_model with no prior value is removed on disconnect", () => {
  const path = tempModelsPath();
  writeFileSync(
    path,
    JSON.stringify({ models: [], default_model: "none" }),
  );
  const original = loadModels(path);
  const backup = snapshotKeys(original, MANAGED_KEYS);

  const input = {
    gateway: { v1: "g/v1", key: "k" },
    model: { id: "a", context: 100 },
    models: [{ id: "a", context: 100 }],
  };
  writeConnected(path, connect(input), "magpie/a");
  assert.equal(loadModels(path).subagent_model, "magpie/a");

  restore(path, backup);
  assert.ok(
    !Object.prototype.hasOwnProperty.call(loadModels(path), "subagent_model"),
    "subagent_model must be removed when it was not there before",
  );
  rmSync(path);
});

test("repeated connect reflects model add/remove and selection change", () => {
  const path = tempModelsPath();
  const gateway = { v1: "g/v1", key: "k" };

  // First connect: two models, one selected.
  let input = {
    gateway,
    model: { id: "a", context: 100 },
    models: [{ id: "a", context: 100 }, { id: "b", context: 200 }],
  };
  writeConnected(path, connect(input), "magpie/a");
  let names = loadModels(path).models.map((m) => m.name);
  assert.deepEqual(names, ["magpie/a", "magpie/b"]);
  assert.equal(loadModels(path).default_model, "magpie/a");

  // Model removed, selection moved.
  input = {
    gateway,
    model: { id: "b", context: 200 },
    models: [{ id: "b", context: 200 }],
  };
  writeConnected(path, connect(input), "magpie/b");
  names = loadModels(path).models.map((m) => m.name);
  assert.deepEqual(names, ["magpie/b"]);
  assert.equal(loadModels(path).default_model, "magpie/b");
  assert.equal(loadModels(path).subagent_model, "magpie/b");
  rmSync(path);
});
