# yi-agent

An AI coding agent written in Rust. Drive it from your terminal in plain language
to read code, change code, and run commands.

- **Terminal first** — a full-screen TUI with Markdown rendering, slash commands,
  and live token/cost readouts
- **Lightweight** — one native binary, no runtime dependencies
- **Controllable** — risky operations ask for confirmation first, with three
  built-in sandbox levels: read-only, workspace-write, and full access

[中文](README.md)

---

## Install

```bash
npm install -g @yi-agent/yi-agent
```

Requires Node.js 14 or newer. The package ships prebuilt binaries for macOS
(Intel / Apple Silicon) and Linux (x64), so no compilation is needed.

On macOS / Linux you can also use Homebrew:

```bash
brew install YichenGong/yi-agent/yi-agent
```

> Behind a slow or partial npm mirror, pass
> `--registry=https://registry.npmjs.org` explicitly. Mirrors lag behind; a
> missing platform subpackage shows up as `Platform package ... not installed`.

The desktop app, offline dmg, and building from source are covered in the user
guide ([docs/guide.md](docs/guide.md), Chinese).

## Quick start

```bash
cd /path/to/your/project
yi-agent
```

The first run walks you through an API key and model identifier, tests the
connection, then writes `~/.yi-agent/models.json`. After that, just say what you
want:

```
> how do I run the tests here? start with the README and CI config
```

It will read files and figure it out on its own, and ask once before running any
command.

## Core features

- **Read and write code** — read, write, apply exact edits, search the repo by
  pattern
- **Run commands** — shell execution; long jobs move to the background with live
  output and one-command termination
- **Look things up** — WebFetch / WebSearch
- **MCP** — connect GitHub, databases, internal systems. Just say "add a GitHub
  MCP" and it writes the config and probes it on the spot
- **Skills** — turn your own runbooks into procedures it can execute
- **Subagents** — fan work out in parallel, watch progress live, message them or
  stop them
- **Auto-compact** — long conversations compact themselves instead of dying at
  the context limit

## Use it with superpowers (recommended)

[superpowers](https://github.com/obra/superpowers) is an open-source library that
turns engineering process into skills: brainstorm the requirement, write a spec
and a plan, implement test-first, then review and verify. yi-agent loads it
as-is.

Clone it and copy the skill directories into your skill root
(`~/.yi-agent/skills/`):

```bash
git clone --depth 1 https://github.com/obra/superpowers.git /tmp/superpowers
mkdir -p ~/.yi-agent/skills
cp -r /tmp/superpowers/skills/* ~/.yi-agent/skills/
```

They appear on your next turn — no restart: `brainstorming`, `writing-plans`,
`executing-plans`, `test-driven-development`, `systematic-debugging`,
`requesting-code-review`, `using-git-worktrees`, `verification-before-completion`,
and seven more (15 in total).

You do not have to remember commands. Describe the task and it will usually start
with brainstorming; you can also name a skill directly:

```
> use brainstorming to pin down what I actually want here
> use systematic-debugging to find out why this test fails
```

To scope skills to one project, put them in `<project>/.yi-agent/skills/`.

## More

The details live in the user guide ([docs/guide.md](docs/guide.md), Chinese):
non-interactive mode, remote access, configuration and file locations,
hand-written MCP config, the desktop app. Module progress is in
[docs/project-management/](docs/project-management/README.md), development rules
in [CLAUDE.md](CLAUDE.md).

## License

MIT
