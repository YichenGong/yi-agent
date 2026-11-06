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
  // Magpie can infer the format from the `.json` name, but declaring it pins
  // the reading so an inferred format can never rewrite models.json as YAML.
  assert.equal(agent.format, "json");
  assert.equal(agent.prefix, _internal.PREFIX);
});

test("agent declares only the Library target Yi Agent has a fixed path for", () => {
  // Yi Agent's user skills root is `~/.yi-agent/skills`; it has no
  // machine-wide instructions file (`AGENTS.md` is per project) nor a fixed
  // MCP config (`<project>/.yi-agent/mcp.json` is per project). Declaring
  // either would make Magpie write to a file Yi Agent never reads.
  assert.deepEqual(agent.library, { skills: "~/.yi-agent/skills" });
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

test("toEntry writes api_style from native_endpoints chat-completions", () => {
  const gateway = { v1: "g/v1", key: "k" };
  const chat = _internal.toEntry(
    { id: "a", native_endpoints: ["/v1/chat/completions"] },
    gateway,
  );
  assert.equal(chat.api_style, "chat");

  // Non-chat native endpoints (responses / messages) leave api_style unset,
  // so Yi Agent's `auto` detection runs instead of pinning a wrong channel.
  const responses = _internal.toEntry(
    { id: "b", native_endpoints: ["/v1/responses"] },
    gateway,
  );
  assert.equal(responses.api_style, undefined);
  const messages = _internal.toEntry(
    { id: "c", native_endpoints: ["/v1/messages"] },
    gateway,
  );
  assert.equal(messages.api_style, undefined);
  const none = _internal.toEntry({ id: "d" }, gateway);
  assert.equal(none.api_style, undefined);
});

test("toEntry writes reasoning_capabilities from model.efforts", () => {
  const gateway = { v1: "g/v1", key: "k" };
  const entry = _internal.toEntry(
    { id: "a", efforts: ["low", "medium", "high", "ultra"] },
    gateway,
  );
  assert.deepEqual(entry.reasoning_capabilities, {
    responses: { supported: ["low", "medium", "high"], probed_at: 0, error: null },
  });
});

test("toEntry puts capabilities on the chat channel when api_style is chat", () => {
  const gateway = { v1: "g/v1", key: "k" };
  const entry = _internal.toEntry(
    { id: "a", efforts: ["low"], native_endpoints: ["/v1/chat/completions"] },
    gateway,
  );
  assert.equal(entry.api_style, "chat");
  assert.deepEqual(entry.reasoning_capabilities, {
    chat: { supported: ["low"], probed_at: 0, error: null },
  });
});

test("toEntry omits reasoning_capabilities when efforts are absent or unusable", () => {
  const gateway = { v1: "g/v1", key: "k" };
  const cases = [
    { id: "a" }, // absent
    { id: "a", efforts: undefined },
    { id: "a", efforts: null },
    { id: "a", efforts: "low" }, // not an array
    { id: "a", efforts: [] }, // empty
    { id: "a", efforts: [5, true, {}] }, // no usable strings
    { id: "a", efforts: ["ultra", "bogus"] }, // nothing in Yi Agent's whitelist
  ];
  for (const model of cases) {
    const entry = _internal.toEntry(model, gateway);
    assert.equal(
      entry.reasoning_capabilities,
      undefined,
      `reasoning_capabilities must be omitted for ${JSON.stringify(model)}`,
    );
  }
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
  // runner, so isolate under this plugin's own directory instead. `.tmp/` is
  // gitignored (see this package's .gitignore).
  const here = dirname(fileURLToPath(import.meta.url));
  const root = join(here, ".tmp");
  mkdirSync(root, { recursive: true });
  return join(mkdtempSync(join(root, "models-")), "models.json");
}

// Remove the whole per-test directory, not just `models.json`: deleting the
// file alone would leave one empty `models-XXXXXX/` behind on every run.
function cleanupModels(path) {
  rmSync(dirname(path), { recursive: true, force: true });
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
  cleanupModels(path);
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
  cleanupModels(path);
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
  cleanupModels(path);
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
  cleanupModels(path);
});

// ---- version compatibility declaration ------------------------------------

// The plugin cannot detect the installed Yi Agent at runtime (moejs has no
// filesystem or process access), so compatibility is a *documented promise*
// rather than a check. These tests keep that promise honest in three ways: the
// declared floor covers every field the mapping writes, the declared
// full-support version covers the optional ones, and the README matrix names
// exactly the same fields.

const PKG = JSON.parse(
  readFileSync(join(dirname(fileURLToPath(import.meta.url)), "..", "package.json"), "utf8"),
);

// `models.json` plus the three top-level keys exist since 0.1.4.
const BASELINE_FIELDS = ["name", "provider", "api_url", "api_key", "model"];
// Optional fields, and the first Yi Agent release that reads each one.
const OPTIONAL_FIELDS = {
  context_length: "0.1.7",
  api_style: "0.1.5",
  reasoning_capabilities: "0.1.8",
};

function cmp(a, b) {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i += 1) {
    if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) - (pb[i] || 0);
  }
  return 0;
}

function manifestFields() {
  const input = {
    gateway: { v1: "https://gw.test/v1", key: "sk-test" },
    models: [
      {
        id: "anthropic/claude-sonnet-4-5",
        context: 200000,
        native_endpoints: ["/chat/completions"],
        efforts: ["low", "high"],
      },
    ],
    agent: {},
  };
  return Object.keys(connect(input).models[0]);
}

test("declared minVersion covers every field the mapping writes", () => {
  const declared = PKG.yiAgent;
  assert.ok(declared, "package.json must declare a yiAgent compatibility block");
  assert.equal(declared.minVersion, "0.1.4");

  // Every field the plugin emits is either baseline or listed as optional.
  const known = new Set([...BASELINE_FIELDS, ...Object.keys(OPTIONAL_FIELDS)]);
  for (const field of manifestFields()) {
    assert.ok(known.has(field), `undeclared field ${field}: update OPTIONAL_FIELDS and the README`);
  }
  // The optional fields are really emitted, so the table is not aspirational.
  for (const field of Object.keys(OPTIONAL_FIELDS)) {
    assert.ok(manifestFields().includes(field), `mapping no longer writes ${field}`);
  }
});

test("declared fullFieldsSince is the newest optional-field release", () => {
  const declared = PKG.yiAgent;
  const newest = Object.values(OPTIONAL_FIELDS).reduce((a, b) => (cmp(a, b) >= 0 ? a : b));
  // If a new optional field needs a newer Yi Agent than the declared
  // full-support version, the declaration is wrong (minVersion may stay).
  assert.equal(declared.fullFieldsSince, newest);
  assert.ok(cmp(declared.minVersion, declared.fullFieldsSince) <= 0);
  // verifiedWith must be a real, already-shipped release the floor trusts.
  assert.ok(cmp(declared.verifiedWith, declared.fullFieldsSince) >= 0);
});

test("README documents the same compatibility floor as package.json", () => {
  const readme = readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), "..", "README.md"),
    "utf8",
  );
  const declared = PKG.yiAgent;
  // The prose must quote the same numbers, so a version bump cannot leave the
  // README telling users a different story than the metadata.
  assert.ok(
    readme.includes(`"minVersion": "${declared.minVersion}"`),
    "README must quote the declared minVersion",
  );
  assert.ok(
    readme.includes(`"fullFieldsSince": "${declared.fullFieldsSince}"`),
    "README must quote the declared fullFieldsSince",
  );
  assert.ok(
    readme.includes(`"verifiedWith": "${declared.verifiedWith}"`),
    "README must quote the declared verifiedWith",
  );
  // Each optional field's owning release should be cited in the table.
  for (const [field, since] of Object.entries(OPTIONAL_FIELDS)) {
    assert.ok(readme.includes(field), `README compatibility table must list ${field}`);
    assert.ok(readme.includes(`v${since}`), `README must cite v${since} for ${field}`);
  }
});

test("each identifier uses its own spelling", () => {
  // Four roles, four spellings. The JSON metadata key is camelCase because
  // package.json spells its own multi-word keys that way; the npm package name
  // is the registry id; `yi-agent` is the machine identifier the binary, the
  // config directory and the env prefix all use; `Yi Agent` is prose.
  assert.equal(typeof PKG.yiAgent, "object", "metadata key must be camelCase `yiAgent`");
  assert.equal(PKG.name, "@yi-agent/magpie-agent-yi-agent", "package name is the registry id");
  assert.equal(agent.id, "yi-agent", "agent.id is the machine identifier");
  assert.equal(agent.bin, "yi-agent");
  assert.equal(agent.name, "Yi Agent", "display name is prose");
  assert.equal(agent.config, "~/.yi-agent/models.json");

  // The metadata must not squat in Magpie's namespace: `magpie` holds only
  // Magpie's own documented fields.
  assert.deepEqual(Object.keys(PKG.magpie).sort(), ["agent", "icon"]);

  // ...and must not be an `engines` entry. Bun installs this plugin, and
  // `engines` is enforceable, so an unknown key there could fail an install
  // instead of merely documenting a version.
  assert.equal(PKG.engines, undefined, "compatibility metadata must not be in `engines`");
});
