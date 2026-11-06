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

The plugin declares **only** `skills`, because the other three Library targets
have no machine-wide location in Yi Agent — a declared path would point Magpie
at a file Yi Agent never reads:

| Library target | Why it is not declared |
| --- | --- |
| `instructions` | Yi Agent reads only `<project>/AGENTS.md`, and deliberately does not search parent directories. There is no home-directory instructions file. |
| `mcp` | The MCP config is per project: `<project>/.yi-agent/mcp.json`. It is not a fixed path. |
| `mcpFormat` | Only meaningful together with `mcp`. |

Two related facts about paths:

- `format: "json"` is declared explicitly even though Magpie infers the format
  from the `.json` file name. The declaration pins the reading, so an inferred
  format can never make Magpie write `models.json` as something else.
- The skills directory follows `YI_AGENT_SKILLS_DIR` when that variable is set;
  the plugin always declares the default `~/.yi-agent/skills`. If you set
  `YI_AGENT_SKILLS_DIR`, Magpie's Library writes go to the default directory
  and Yi Agent will not see them. (Model entries are unaffected: those live in
  `models.json`, see the next section.)

## Version compatibility

This package is versioned independently of Yi Agent, but not arbitrarily:
what it writes has to mean something to the Yi Agent that reads it. The two are
**coupled by declaration, decoupled by release** — the plugin states which Yi
Agent it is built against, and each side ships on its own schedule.

`package.json` carries the machine-readable claim:

```json
"yiAgent": { "minVersion": "0.1.4", "fullFieldsSince": "0.1.8", "verifiedWith": "0.1.13" }
```

Magpie and npm both ignore unknown top-level fields, so this is metadata only.
Nothing enforces it at runtime: the plugin cannot detect the installed Yi Agent
version (Magpie's agent runtime has no filesystem or process access), so the
declaration is a promise kept by the release process, not a check.

### What to call it

The four spellings are not alternatives for one thing — they are four different
identifiers with four different jobs. Mixing them up is what makes the name look
arbitrary:

| Role | Standard spelling | Where it appears |
| --- | --- | --- |
| npm package (registry id) | `@yi-agent/yi-agent` | `dependencies`, `npm install`, `require` |
| Display name (read by people) | `Yi Agent` | docs, UI, `agent.name` |
| Machine identifier (binary, id, directory) | `yi-agent` | `agent.id`, `bin`, `~/.yi-agent`, repo name, `YI_AGENT_*` env prefix |
| JSON key (metadata namespace) | `yiAgent` | this package's top-level metadata |

So the field is `yiAgent`, and the reason is the fourth row: it is a **JSON
key**, not a package name and not a display name. package.json spells its own
multi-word keys in camelCase (`devDependencies`, `peerDependencies`,
`publishConfig`, `optionalDependencies`); a hyphenated top-level key would be
the only one in the file. `@yi-agent/yi-agent` is the registry identifier — it
names what you install, and would be meaningless here. `Yi Agent` is prose.

Two places the metadata must **not** go:

- **Not inside `magpie`.** That object is Magpie's namespace (`magpie.agent`,
  `magpie.icon`); putting our data there claims another project's keys.
- **Not in `engines`.** It looks tempting — `engines` is the standard field for
  "runtimes this works with" — but it can only express one range, and package
  managers are allowed to *enforce* it. Bun installs this plugin (`bun add`),
  so an unknown engine key that some future version decides to check would turn
  a compatibility note into a failed install. A key nothing reads cannot fail.

Verified: `npm pack` keeps both custom blocks in the tarball, so publishing does
not strip them.

### What each field needs

`models.json` and the three top-level keys (`models`, `default_model`,
`subagent_model`) exist since **v0.1.4**. The per-entry fields arrived later:

| Written field | Needs Yi Agent | Effect on an older Yi Agent |
| --- | --- | --- |
| `name`, `provider`, `api_url`, `api_key`, `model` | v0.1.4 | — |
| `context_length` | v0.1.7 | Ignored; compaction threshold falls back to the built-in 200000 default |
| `api_style` | v0.1.5 | Ignored; `auto` detection runs, costing one extra probe round-trip per model |
| `reasoning_capabilities` | v0.1.8 | Ignored; effort tiers are treated as unknown and stripped at request time |

Unknown fields are **silently ignored**, not rejected: `ModelEntry` has no
`#[serde(deny_unknown_fields)]` in any released version. So one plugin build
works against every Yi Agent from 0.1.4 on — it just degrades as the table
says. `verifiedWith` is the release this build was actually exercised against.

Two degradations are worth stating plainly rather than hiding in a table:

- **Before 0.1.7, `context_length` is not read at all.** Yi Agent assumes
  200000 tokens, so a Magpie model with a smaller real window will only be
  compacted past 200k — requests can be rejected for exceeding the model's
  context. If you use such models, run Yi Agent 0.1.7 or newer.
- **Before 0.1.8, reasoning efforts are not declared.** Yi Agent lists all
  tiers and relies on stripping-and-retrying, so an unsupported effort fails a
  request before it succeeds. It still works; it is just noisier.

### When to publish which version

The plugin and Yi Agent are released on separate tracks. Which track a change
belongs to follows from what actually changed:

| Change | Yi Agent release? | Plugin release? |
| --- | --- | --- |
| Fix a mapping bug, a notice string, or docs | No | Patch (`0.2.x`) |
| Add support for a Yi Agent field that already shipped | No | Patch (`0.2.x`), update `minVersion`/`fullFieldsSince` |
| Adapt to a **new** Yi Agent field | Yes | Minor (`0.x.0`), same cycle, bump `verifiedWith` |
| Yi Agent ships features the plugin does not map | Yes | **No** — nothing in the plugin changed |
| Breaking change to a field this plugin writes | Yes | Major, and raise `minVersion` |

The third and fourth rows are the ones people get wrong in both directions: a
plugin minor only exists to follow a new field, and a Yi Agent release alone is
never a reason to republish the plugin. Publishing a new plugin version
therefore means "the mapping changed", never "the agent moved".

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
  (An override that does not end in `.yi-agent` gets `.yi-agent` appended, so
  `YI_AGENT_HOME=/opt/yi` reads `/opt/yi/.yi-agent/models.json`; none of those
  layouts match the literal path either.) Declaring `$YI_AGENT_HOME/models.json`
  instead is not an option either: Magpie expands a `$VAR` in a declared path,
  and an unset variable expands to an empty string, which would leave Magpie
  writing `/models.json`. Use the default location, or leave this plugin off.
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
