// Magpie agent plugin for Yi Agent.
//
// Yi Agent keeps its machine-wide model catalog in `~/.yi-agent/models.json`
// (see `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`). This plugin makes
// Magpie's model directory available to Yi Agent by rewriting that file while
// the agent is connected, and relies on Magpie's per-key backup/restore to put
// it back on disconnect.
//
// This file runs in Magpie's agent runtime (moejs), NOT Node.js or Bun:
//   - no Node APIs (fs, process, fetch, timers)
//   - imports are limited to files inside this plugin
//   - loading and connect() each get one second
// So it is a pure, dependency-free mapping with no I/O.
//
// Deliberately there is no `main`/`exports` in package.json: a file referenced
// only by `magpie.agent` is loaded as an agent. A `main` would make Magpie load
// it as a provider too and call every exported function as a plugin, which
// would break `connect`.

// Written before a Magpie model id. `agent.prefix` and `connect()` must agree
// on this: Magpie writes `default_model = prefix + model.id`, and the entry we
// emit must carry that exact string as its `name`, or Yi Agent can't resolve it.
const PREFIX = "magpie/";

// `context_length` in Yi Agent is Option<u32>. A value outside this range would
// make serde reject the whole entry, so it is omitted instead of passed through.
const U32_MAX = 4294967295;

export const agent = {
  id: "yi-agent",
  name: "Yi Agent",
  bin: "yi-agent",
  // The file Magpie edits. `~` is expanded by Magpie to the user's HOME.
  config: "~/.yi-agent/models.json",
  // The key of the model in models.json.
  model: "default_model",
  prefix: PREFIX,
  notice:
    "A running yi-agent TUI does not hot-reload models.json — restart it, or run /model default after connecting or switching, to pick up the new model.",
};

// A Magpie model id (e.g. "anthropic/claude-sonnet-4-5") mapped to a Yi Agent
// catalog entry. Every model is surfaced as provider "openai" pointing at the
// gateway's OpenAI-compatible `/v1` endpoint; the gateway translates the request
// to whatever API the model speaks.
function toEntry(model, gateway) {
  const id = model && typeof model.id === "string" ? model.id : "";
  const entry = {
    name: PREFIX + id,
    provider: "openai",
    api_url: gateway.v1,
    api_key: gateway.key,
    model: id,
  };
  const context = model && model.context;
  if (
    typeof context === "number" &&
    Number.isInteger(context) &&
    context > 0 &&
    context <= U32_MAX
  ) {
    entry.context_length = context;
  }
  return entry;
}

// `connect(input)` returns the keys Magpie writes beside the model. It is called
// again when Magpie's models change, so the `models` array it returns stays in
// sync with Magpie's directory. It receives:
//   gateway: { url, v1, key }   — the gateway address and the agent's own key
//   model:   the currently selected model (id, name, context, output, …)
//   models:  every model Magpie offers the agent, in the same shape
//   agent:   { id, config }
export function connect(input) {
  const gateway = input.gateway || {};
  const model = input.model;
  const list = Array.isArray(input.models) ? input.models : [];
  const entries = list
    .map((m) => toEntry(m, gateway))
    .filter((e) => e.model !== "");

  const result = { models: entries };

  // default_model is written by Magpie as `prefix + model.id`. subagent_model
  // must name an entry that exists in `models`, so while connected it points at
  // the same selected model; Magpie backs up the original value and restores it
  // (or removes the key, when it wasn't there) on disconnect. Without this,
  // a subagent_model pointing at an original entry (which is temporarily out of
  // the array) would be a dangling reference.
  const selectedId = model && typeof model.id === "string" ? model.id : "";
  if (selectedId !== "") {
    result.subagent_model = PREFIX + selectedId;
  }

  return result;
}

// Test seams only. Magpie ignores every export other than `agent`/`connect`.
export const _internal = { PREFIX, toEntry };
