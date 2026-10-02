# yi-agent

An AI coding agent written in Rust. Drive it from your terminal in plain language
to read code, change code, and run commands.

- **Terminal first** — a full-screen TUI with Markdown rendering, slash commands,
  and live token/cost readouts
- **Lightweight** — one native binary, no runtime dependencies; plus a resident
  daemon and a desktop app
- **Controllable** — risky operations ask for confirmation first, with three
  built-in sandbox levels: read-only, workspace-write, and full access

[中文](README.md)

---

## What it can do

- Read and modify your codebase: read files, write files, apply exact edits,
  search by pattern across the repo
- Run shell commands; long jobs can be managed in the background, with live
  output and one-command termination
- Look things up on the web (WebFetch / WebSearch)
- Connect external tools over MCP — GitHub, databases, internal systems
- Load Skills, turning your own runbooks into procedures it can execute
- Spawn subagents to work in parallel, watch their progress live, message them
  or stop them
- Auto-compact the conversation when it outgrows the context window

## Install

### Homebrew (macOS / Linux)

```bash
brew install YichenGong/yi-agent/yi-agent
```

### npm

```bash
npm install -g @yi-agent/yi-agent
```

The npm package ships prebuilt binaries for macOS (Intel / Apple Silicon) and
Linux (x64), so no compilation is needed.

### Direct download

Grab the `.tar.gz` for your platform from
[Releases](https://github.com/YichenGong/yi-agent/releases), unpack it, and put
`yi-agent` on your `PATH`.

### From source

Requires Rust 1.85 or newer.

```bash
git clone https://github.com/YichenGong/yi-agent.git
cd yi-agent/yi-agent-rs
cargo build --release
# binary: target/release/yi-agent
```

Verify the install:

```bash
yi-agent --version
```

## Quick start

**1. Give it a model API key.**

```bash
export MODEL_API_KEY=sk-ant-...
```

Anthropic is the default. To use OpenAI or another compatible endpoint, see
"Switching models" below.

**2. Go to your project and run it.**

```bash
cd /path/to/your/project
yi-agent
```

**3. Just talk to it.**

```
> how do I run the tests here? start with the README and CI config
```

It will read files and figure it out on its own, and ask once before running any
command.

> Exporting the key in every new shell gets old. See "Where configuration lives"
> for persistent setup.

## Three ways to use it

### 1. Interactive TUI (recommended)

```bash
yi-agent
```

This is the flow above. A few handy controls:

| Control | Effect |
| --- | --- |
| Type `/` | Open the command menu: `/help` `/model` `/cost` `/clear` `/compact` `/config` `/runtime` `/mcp` `/quit` |
| `Ctrl+P` | Open the runtime panel: bash tasks / managed processes / subagent traces |
| `Esc` | Interrupt the current turn (does not quit) |
| `Ctrl+C` twice | Quit |

`/cost` shows what this session has spent; `/compact` compacts history manually.
Long conversations compact automatically.

### 2. Non-interactive mode (scripts / CI)

Hand it one prompt, get an answer, exit. Good for scripts and pipelines:

```bash
yi-agent run "collect every TODO comment under src/ into a list"

# JSONL output, easy to process (one event per line)
yi-agent run --json "count the Rust lines in this repo" | jq -r 'select(.AssistantText) | .AssistantText'

# read the prompt from a pipe
echo "explain the deploy target in the Makefile" | yi-agent run
```

### 3. Desktop app (macOS)

A native window with conversations, tool-call cards, approval prompts, and
subagents. See [desktop/README.md](desktop/README.md) for setup and development.

## Remote access (experimental)

`app-server` can serve over WebSocket for network clients. **Every connection must
authenticate with a paired device token** (`ws://host/ws?token=<t>` or
`Authorization: Bearer <t>`; a missing/invalid token is closed with ws code
`4401`), so multiple clients can connect at once:

```bash
yi-agent app-server --listen ws://127.0.0.1:8790   # warns when bound beyond loopback
```

The same sessions can be viewed and controlled from several devices: QR-style
pairing (one-time `XXXX-XXXX` code, valid for 5 minutes, new devices default to
`control` scope), device listing/revocation, and approvals broadcast to every
connected device.

A client can call `thread/subscribe {threadIds}` to **receive only the sessions
it cares about** (clients that never call it still receive everything); once
subscribed, the per-token stream is coalesced (flushed every 100ms or 4KB,
never merged across items, order preserved) to cut the traffic and noise on a
phone over the WAN. See the
[thread-scoped subscription design](docs/superpowers/specs/2026-10-02-thread-subscription-filtering-design.md).

Delivery is split into three IM-style layers: the **list layer**
(`thread/started`, `thread/status/updated`) and the **global layer**
(`ui/settings/updated`, `error`, …) always reach every client, so the sidebar's
session list and run status stay live on any device; only the **content layer**
(`item/*`, `turn/*`, and the rest of the transcript) is filtered by
`thread/subscribe`. A remote (iOS) client therefore keeps just an LRU warm
window of at most 8 threads, rolling its subscription as the user selects them;
threads outside the window stay quiet. When the user returns to a **running**
cold thread the client catches it up with the read-only
`thread/readItems {threadId, afterItemId?}` instead of `thread/resume`, which
would interrupt the in-flight turn. The desktop (stdio) client never subscribes
and so always receives everything.

Pairing supports **QR codes**: the desktop's "Remote access" settings page shows a QR
code beside the pairing code once a relay URL is entered, and
`yi-agent pair code --relay <url>` prints one in the terminal; the iOS pairing
screen's "Scan" button reads it and pairs automatically. The payload is
`yiagent://pair?v=1&relay=<url>&code=<code>` — it carries the relay URL and the
one-time pairing code, **never a token**. Manual text entry still works.

> **Experimental**: the pairing flow is now **wired end-to-end** — a
> `pair/create` code is **persisted** to `~/.yi-agent/pairing.json`, so a code
> minted by the desktop stdio process can be redeemed by the separate
> `--relay`/`ws://` process; iOS gets a first-launch pairing form (relay URL +
> code) and the desktop settings gain a "Remote access" tab to mint codes and
> manage devices. Known limits: **no QR scanning** yet (type the code) and **no
> installable iOS build** (blocked on the Xcode runtime/signing). See
> [iOS remote control and relay deployment](docs/relay-deploy.md).

**The iOS app connects through a self-hosted reverse WSS relay** (both ends only
make outbound connections; the computer opens no inbound port):

```bash
yi-agent app-server --relay 'wss://relay.example.com/connect?session=<id>'
```

See [iOS remote control and relay deployment](docs/relay-deploy.md) (Chinese) for
relay deployment, domain + TLS, iOS build and pairing, and troubleshooting. Design
rationale: [mobile remote access design](docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md)
(Tier 1).

## Where configuration lives

Priority, lowest to highest:

1. Built-in defaults
2. Global config `~/.yi-agent/.env`
3. Project config `<project>/.yi-agent/.env` (overrides global)
4. CLI flags / environment variables (highest)

Config never lands in your project root — it goes in `.yi-agent/.env`, so your
repo stays clean.

**Easy path**: run `yi-agent web` and fill things in from the browser
(<http://127.0.0.1:7292> by default). It writes the files above for you.

## Common settings

| Goal | Environment variable | CLI flag |
| --- | --- | --- |
| API key | `MODEL_API_KEY` | `--api-key` |
| Switch provider | `YI_AGENT_PROVIDER` (`anthropic` / `openai`) | `--provider` |
| Switch model | `YI_AGENT_MODEL` | `--model` |
| Custom API endpoint | `MODEL_API_URL` | `--api-url` |
| Set the working directory | `YI_AGENT_WORKDIR` | `--workdir` |
| Max steps per turn | `YI_AGENT_MAX_TURNS` (default 200) | `--max-turns` |
| Auto-compact threshold | `YI_AGENT_COMPACT_RATIO` (default 80, percent of context) | `--compact-ratio` |
| Web search | `BOCHA_API_KEY` | — |

**Switching models**, for example to OpenAI:

```bash
yi-agent --provider openai --model gpt-4o
```

**About permission prompts.** By default every risky operation asks for
confirmation. These are the startup switches:

- `--yolo` (or `--dangerously-skip-permissions`): skip confirmations. Recommended
  only in containers or disposable environments
- `--sandbox read-only`: the whole process is read-only; nothing can be modified
- `--sandbox workspace-write`: writes limited to the workspace, network restricted

Blacklisted commands are blocked in every mode.

**About the subagent runtime.** The first time you delegate to a subagent in the
TUI it asks whether to start the local runtime; afterwards, `/runtime always` /
`/runtime never` / `/runtime ask` control whether it starts automatically.

## Project layout and progress

The code lives in `yi-agent-rs/` (a Rust workspace with 11 crates); the desktop
client lives in `desktop/`. Per-module completion status, verification commands,
and maintenance rules are in
[docs/project-management/](docs/project-management/README.md).

## More documentation

- Known issues: [docs/bug-list.md](docs/bug-list.md)
- Design docs: [docs/plans/](docs/plans/)
- Contributing: [CLAUDE.md](CLAUDE.md) (branching, commits, testing rules)

## License

MIT
