# @yi-agent/magpie-agent-yi-agent

A [Magpie](https://usemagpie.ai) agent plugin that connects
[Yi Agent](https://github.com/YichenGong/yi-agent) to Magpie's gateway.

Install it with:

```bash
magpie plugin add @yi-agent/magpie-agent-yi-agent
```

Then find **Yi Agent** on Magpie's Agents page, pick one of Magpie's models in
its row, and the plugin writes that model into Yi Agent's config. Switch it off
and everything goes back to how you had it.

## What it does

Yi Agent keeps its machine-wide model catalog in `~/.yi-agent/models.json` (a
JSON object with `models`, `default_model` and `subagent_model`). While the
agent is connected, Magpie rewrites that file:

| Yi Agent field | Value written while connected |
| --- | --- |
| `models[].name` | `magpie/` + the Magpie model id (e.g. `magpie/anthropic/claude-sonnet-4-5`) |
| `models[].provider` | `openai` |
| `models[].api_url` | `gateway.v1` (the gateway's OpenAI-compatible `/v1` root) |
| `models[].api_key` | the key Magpie assigned to Yi Agent |
| `models[].model` | the Magpie model id, passed through unchanged |
| `models[].context_length` | filled in when Magpie reports a valid positive context length |
| `default_model` | written by Magpie as `magpie/` + the selected model id |
| `subagent_model` | `magpie/` + the selected model id while connected |

The gateway translates requests to whatever API each model speaks, so Yi Agent
talks to every Magpie model through one OpenAI-compatible endpoint.

## Things to know

- **Two icons, two sources.** Magpie's *Installed* list draws a plugin's logo
  from `package.json`'s `magpie.icon`; the *Agents* page draws each agent's row
  icon from the agent module's own exported `icon` field and does not fall back
  to `magpie.icon`. Both point at the same PNG here, so the two rows match — keep
  them in sync if you ever change the artwork (the tests compare them).

- **`models` is an array owned by Magpie while connected.** While the agent is
  connected, the `models` array holds only the Magpie models above; your
  original Yi Agent entries are temporarily out of it. Magpie backs them up and
  restores them on disconnect. This means edits you make to `models.json` by
  hand *while connected* are on Magpie-managed keys and will be overwritten on
  the next model change. Edit your own model entries after disconnecting (or
  switch the agent off first).
- **`subagent_model` is also managed during connection.** If it pointed at one
  of your original entries, that entry is out of the array while connected, so
  the plugin points `subagent_model` at the current Magpie model instead. On
  disconnect Magpie restores your original value (or removes the key if it
  wasn't there).
- **Path: a custom `YI_AGENT_HOME` is not supported.** The config path is
  fixed at `~/.yi-agent/models.json`. Yi Agent also reads `YI_AGENT_HOME`, but
  this plugin does not: it declares the literal `~/.yi-agent/models.json`, so
  setting `YI_AGENT_HOME` for both programs does not help — a Yi Agent started
  with it reads `$YI_AGENT_HOME/models.json` and never sees what Magpie writes.
  Declaring `$YI_AGENT_HOME/models.json` instead is not an option either:
  Magpie expands a `$VAR` in a declared path, and an unset variable expands to
  an empty string, which would leave Magpie writing `/models.json`. Use the
  default location, or leave this plugin off.
- **A running TUI does not hot-reload the model.** Yi Agent's TUI reads
  `models.json` at startup and re-reads it only when you switch the model with
  `/model`. After connecting, switching models, or disconnecting in Magpie, a
  TUI that was already running keeps using the old model — restart it, or run
  `/model default` to pick up the change.

## Development

```bash
cd integrations/magpie-agent-yi-agent
npm test            # node --test tests/agent.test.mjs
```

The plugin entry (`yi-agent.agent.js`) runs in Magpie's restricted agent
runtime (not Node.js and not Bun): it has no dependencies, no Node APIs, and no
I/O. The tests exercise the pure mapping directly, plus a simulation of
Magpie's per-key write protocol (backup → write → restore) covering: no
existing config, existing entries, switching the default model, model
add/remove, repeated `connect()`, disconnect restoration, `subagent_model`
with/without a prior value, and unusable model metadata.

## Publishing

This package is versioned independently of Yi Agent. To publish a new version:

```bash
cd integrations/magpie-agent-yi-agent
npm version patch   # or minor / major
git push            # CI publishes the new version when it lands on main
```

CI (`.github/workflows/magpie-agent-yi-agent.yml`) publishes only when the version
actually changed — an already-published version is skipped, so re-merging
without bumping doesn't republish. You can also publish by hand:

```bash
cd integrations/magpie-agent-yi-agent
npm publish --access public
```

Community-market listing is separate from npm publication and is not
automatic; see
[magpie-community/plugins](https://github.com/magpie-community/plugins) for how
to request listing if you want it.
