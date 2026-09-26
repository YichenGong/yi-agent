# yi-agent Desktop

Native macOS desktop app for `yi-agent`, built with **Tauri 2.x** (Rust backend)
and **React 19 + TypeScript + Tailwind CSS v4 + Vite** (frontend).

The Tauri Rust backend (`src-tauri/`) contains **no agent logic** — it only
manages the `yi-agent app-server` sidecar process lifecycle and bridges JSON-RPC
2.0 frames between the frontend and the sidecar's stdio. The frontend depends
only on the wire protocol, never on Rust types.

This app is **independent of the Rust workspace** at `yi-agent-rs/` and is not a
member of that workspace.

## Prerequisites

- Node.js 20+ and npm
- Rust toolchain (`rustc`, `cargo`) with the host target triple
- macOS with Xcode command line tools (for the native window / bundling)

## Development

Install JavaScript dependencies:

```bash
npm install
```

Build the `yi-agent` sidecar binary and start the app in dev mode:

```bash
npm run sidecar      # builds yi-agent (debug) into src-tauri/binaries/
npm run tauri dev    # opens the native window with hot reload
```

`npm run tauri dev` launches Vite on <http://localhost:1420> (`strictPort`) and
runs the Tauri Rust backend against it.

## Build

Frontend-only production build (TypeScript check + Vite → `dist/`):

```bash
npm run build
```

Full native bundle (`.app` + `.dmg`) for the host platform:

```bash
npm run sidecar:release   # builds yi-agent in release mode
npm run tauri build
```

## Tests

Frontend unit tests (Vitest):

```bash
npm test
```

Rust backend tests:

```bash
cd src-tauri && cargo test
```

## Sidecar (Task P2)

The Tauri backend spawns the `yi-agent` CLI as a bundled **sidecar** and speaks
JSON-RPC 2.0 over its stdio. Tauri's `externalBin` mechanism requires the binary
to be copied into `src-tauri/binaries/` with the host target-triple suffix
(e.g. `yi-agent-aarch64-apple-darwin`).

`desktop/scripts/build-sidecar.sh` automates that build-and-copy step and is
exposed through two npm scripts:

- `npm run sidecar` — debug build of `yi-agent`, copied to
  `src-tauri/binaries/yi-agent-<target-triple>`
- `npm run sidecar:release` — release build, same destination

Run one of these **before** `npm run tauri dev` or `npm run tauri build`, since
the app will otherwise fail to spawn its sidecar. `src-tauri/binaries/` is
gitignored.

## Layout

```
desktop/
  index.html            # Vite entry HTML
  package.json          # npm scripts + JS deps
  postcss.config.js     # Tailwind v4 PostCSS plugin
  vite.config.ts        # Vite config (port 1420, strictPort)
  src/                  # React + TypeScript frontend
  src-tauri/            # Tauri Rust backend (sidecar lifecycle + RPC bridge)
  scripts/              # build-sidecar.sh (Task P2)
```

## Verification status

Frontend unit tests (`npm test`, 14 tests), the frontend production build
(`npm run build`), and the Rust backend tests (`cd src-tauri && cargo test`) are
green.

The native bundle is built and verified: `npm run sidecar:release && npm run
tauri build` produces `src-tauri/target/release/bundle/macos/yi-agent.app` (with
the sidecar embedded at `Contents/MacOS/yi-agent`) and
`src-tauri/target/release/bundle/dmg/yi-agent_0.1.0_aarch64.dmg`. Launching the
`.app` spawns the `yi-agent app-server --listen stdio://` child process.

The **manual end-to-end smoke checklist** (streaming text, tool cards, approval
allow/deny, Stop interrupt, sidecar-kill banner) still requires human
interaction — see `docs/project-management/desktop.md`:

```bash
npm run sidecar && npm run tauri dev             # manual end-to-end smoke
```
