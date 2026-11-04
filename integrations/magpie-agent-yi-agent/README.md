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
| `models[].api_style` | `chat` when Magpie reports a native `/chat/completions` endpoint, else unset (Yi Agent's `auto` detection runs) |
| `models[].reasoning_capabilities` | the model's reasoning-effort tiers on the channel Yi Agent will use, when Magpie reports any |
| `default_model` | written by Magpie as `magpie/` + the selected model id |
| `subagent_model` | `magpie/` + the selected model id while connected |

The gateway translates requests to whatever API each model speaks, so Yi Agent
talks to every Magpie model through one OpenAI-compatible endpoint.

### Reasoning efforts and the native endpoint

Magpie already knows each model's reasoning-effort tiers (`model.efforts`) and
its native gateway path (`model.native_endpoints`). The plugin folds both into
the catalog entry so Yi Agent doesn't have to discover them by round-tripping:

- `model.efforts` is mapped to `reasoning_capabilities` on the channel Yi Agent
  resolves for the entry (`chat` when `api_style` is pinned to chat, else
  `responses`). Only tiers in Yi Agent's seven-value whitelist
  (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`) are kept; a value like
  `ultra` that Magpie reports for some models is dropped. `probed_at` is written
  as `0`: Yi Agent reads trust from a non-error record, not from the timestamp.
- `model.native_endpoints` containing `/chat/completions` pins `api_style` to
  `chat`, so Yi Agent skips its `/responses`-first probe for those models.
  Responses- or Messages-native models are left unset (that is Yi Agent's
  `auto` default already).

### Library skills

Yi Agent already discovers `SKILL.md` folders under `~/.yi-agent/skills`. The
plugin declares that directory as its Library `skills`, so skills the user
turns on in Magpie's Library are written there and picked up by Yi Agent with
no further configuration.

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
  `models.json` at startup, and its `/model` list is that startup snapshot — it
  never re-reads the file while running. After connecting, switching models, or
  disconnecting in Magpie, an already-running TUI therefore:
  - keeps using the old model, and
  - cannot switch to a `magpie/*` entry by name: `/model magpie/…` answers
    "unknown model name", because the entries were not in the list it loaded.

  `/model default` still works while running: it clears the session override and
  the driver resolves `default_model` against a fresh read of the file. So
  `/model default` picks up a new Magpie default, but to *choose* a different
  `magpie/*` model by name, restart the TUI first.
- **The first connection creates `models.json` with mode 0644.** Yi Agent writes
  that file itself as 0600 because it holds API keys; Magpie creates it with its
  own umask when the file does not exist yet (it keeps 0600 when the file
  already existed). After your very first connect, run
  `chmod 600 ~/.yi-agent/models.json` to restore the tighter permissions.
- **`ua` is declared but not yet effective.** The agent declares
  `ua: ["yi-agent"]` so Magpie can name Yi Agent's requests in Usage. Yi Agent's
  model providers currently send no `User-Agent` header, so the field only
  starts matching once they do; until then Magpie falls back to the agent's key.

## Development

```bash
cd integrations/magpie-agent-yi-agent
npm test            # node --test tests/agent.test.mjs
```

Tests write scratch files under `tests/.tmp/` (gitignored) and remove them when
each test finishes.

The plugin entry (`yi-agent.agent.js`) runs in Magpie's restricted agent
runtime (not Node.js and not Bun): it has no dependencies, no Node APIs, and no
I/O. The tests exercise the pure mapping directly, plus a simulation of
Magpie's per-key write protocol (backup → write → restore) covering: no
existing config, existing entries, switching the default model, model
add/remove, repeated `connect()`, disconnect restoration, `subagent_model`
with/without a prior value, unusable model metadata, `api_style` from
`native_endpoints`, and `reasoning_capabilities` from `model.efforts`.

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
