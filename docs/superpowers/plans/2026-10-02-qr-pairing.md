# QR-Code Pairing (S4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the desktop/CLI display a QR code and the iOS app scan it to pair, replacing manual entry of the relay URL and pairing code.

**Architecture:** A single URI payload (`yiagent://pair?v=1&relay=<enc>&code=<code>`) is the only cross-end contract, implemented as a pure function on each side with shared fixtures. The desktop settings page and `yi-agent pair code` render it; the iOS pairing screen scans it (in-app camera + `jsqr`), parses it, and auto-redeems via the existing `PairingScreen` seams.

**Tech Stack:** Rust (`url`/`form_urlencoded`, `qrcode` 0.14), React + TypeScript (npm `qrcode@1.5.4`, `jsqr@1.4.0`), Tauri 2 iOS (`Info.ios.plist`).

## Global Constraints

- **Environment (every Rust command):**
  ```bash
  export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
  export CARGO_TARGET_DIR=/tmp/yi-s4
  ```
- **Payload format (exact, both sides):** `yiagent://pair?v=1&relay=<percent-encoded>&code=<code>`. Query encoding is **form encoding** (`application/x-www-form-urlencoded`: space→`+`; `* - . _` and alphanumerics unescaped; everything else `%XX` uppercase). Both implementations MUST use form encoding so bytes match.
- **Conventional commits.** NEVER add `Co-Authored-By`.
- **rustfmt:** only via `rustfmt --edition 2024 --check --config skip_children=true <changed-file>`, and only on files you changed. Never sweep pre-existing drift into a commit.
- **Version floors (verbatim from spec):** npm `qrcode@1.5.4`, npm `jsqr@1.4.0`, Rust `qrcode@0.14.1`.
- **Gates (run before each commit):**
  ```bash
  cargo test -p yi-agent-app-server
  cargo test -p yi-agent
  cd desktop && npx vitest run && npx tsc --noEmit
  ```
- **Hard invariant:** the text-entry pairing path must never regress; a failing scan path must leave the form usable.

---

### Task 1: Rust pair-URI codec (`pair_uri.rs`)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/pair_uri.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` (add `pub mod pair_uri;` after `pub mod pairing;`)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` (add `url.workspace = true`, `form_urlencoded = "1"`)

**Interfaces:**
- Produces:
  - `pub fn build_pair_uri(relay: &str, code: &str) -> String`
  - `pub fn parse_pair_uri(text: &str) -> Option<(String, String)>` — returns `(relay, code)` or `None`.
  - `pub const PAIR_SCHEME: &str = "yiagent";` `pub const PAIR_HOST: &str = "pair";` `pub const PAIR_VERSION: &str = "1";`

- [ ] **Step 1: Write the failing test**

Append to the new module (see Step 3 for the module file; the test lives at the bottom of it):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // 与 TS 侧 `desktop/src/lib/pairUri.ts` 的用例**逐字节**共用同一组 fixture。
    #[test]
    fn builds_the_canonical_uri() {
        assert_eq!(
            build_pair_uri("wss://relay.example.com/ws?session=abc", "ABCD-EFGH"),
            "yiagent://pair?v=1&relay=wss%3A%2F%2Frelay.example.com%2Fws%3Fsession%3Dabc&code=ABCD-EFGH"
        );
        assert_eq!(
            build_pair_uri("ws://192.168.1.5:8080/ws", "WXYZ-1234"),
            "yiagent://pair?v=1&relay=ws%3A%2F%2F192.168.1.5%3A8080%2Fws&code=WXYZ-1234"
        );
        // 空格/`&`/`.`：钉住 form-encoding 语义（空格→`+`，`.` 不转义）。
        assert_eq!(
            build_pair_uri("wss://r/a b.c?x=1&y=2", "A-B"),
            "yiagent://pair?v=1&relay=wss%3A%2F%2Fr%2Fa+b.c%3Fx%3D1%26y%3D2&code=A-B"
        );
    }

    #[test]
    fn round_trips() {
        for (relay, code) in [
            ("wss://relay.example.com/ws?session=abc", "ABCD-EFGH"),
            ("ws://192.168.1.5:8080/ws", "WXYZ-1234"),
        ] {
            assert_eq!(parse_pair_uri(&build_pair_uri(relay, code)), Some((relay.into(), code.into())));
        }
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert_eq!(parse_pair_uri("hello"), None);
        assert_eq!(parse_pair_uri("https://pair?v=1&relay=wss%3A%2F%2Fr&code=C"), None); // wrong scheme
        assert_eq!(parse_pair_uri("yiagent://pair?v=2&relay=wss%3A%2F%2Fr&code=C"), None); // wrong version
        assert_eq!(parse_pair_uri("yiagent://pair?v=1&relay=wss%3A%2F%2Fr"), None); // missing code
        assert_eq!(parse_pair_uri("yiagent://pair?v=1&code=C"), None); // missing relay
        assert_eq!(parse_pair_uri("yiagent://pair?v=1&relay=http%3A%2F%2Fr&code=C"), None); // not ws(s)
        assert_eq!(parse_pair_uri("yiagent://pair?v=1&relay=&code=C"), None); // empty relay
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib pair_uri
```
Expected: FAIL — `unresolved import` / `cannot find module pai_uri` (module not created yet).

- [ ] **Step 3: Write minimal implementation**

Create `yi-agent-rs/crates/yi-agent-app-server/src/pair_uri.rs`:

```rust
//! 二维码配对载荷编解码（S4）。
//!
//! 二维码文本 = `yiagent://pair?v=1&relay=<percent-encoded relay url>&code=<code>`。
//! 这是**两端唯一的跨端契约**：TS 侧 `desktop/src/lib/pairUri.ts` 必须逐字节一致
//! （两份用同一组 fixture 断言）。纯函数、无 IO。
//!
//! `relay` 既可是中继地址（`wss://relay.example.com/ws?session=<id>`），也可是局域网
//! 直连地址（`ws://192.168.x.x:8080/ws`）；因自身含 `?`，作 query 值须 form-encode。

/// 自定义 scheme（不依赖任何 OS 级 URL scheme 注册）。
pub const PAIR_SCHEME: &str = "yiagent";
/// scheme 下的 host 段。
pub const PAIR_HOST: &str = "pair";
/// 载荷版本位；未知版本一律判无效。
pub const PAIR_VERSION: &str = "1";

/// 按契约拼出二维码文本。查询串用 form 编码（与浏览器 `URLSearchParams` 一致）。
pub fn build_pair_uri(relay: &str, code: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("v", PAIR_VERSION);
    q.append_pair("relay", relay);
    q.append_pair("code", code);
    format!("{PAIR_SCHEME}://{PAIR_HOST}?{}", q.finish())
}

/// 解析二维码文本；不满足契约（scheme/host/版本/字段/ws 前缀任一不符）返回 `None`。
pub fn parse_pair_uri(text: &str) -> Option<(String, String)> {
    let u = url::Url::parse(text).ok()?;
    if u.scheme() != PAIR_SCHEME || u.host_str() != Some(PAIR_HOST) {
        return None;
    }
    let mut relay = None;
    let mut code = None;
    let mut version = None;
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "v" => version = Some(v.into_owned()),
            "relay" => relay = Some(v.into_owned()),
            "code" => code = Some(v.into_owned()),
            _ => {}
        }
    }
    if version.as_deref() != Some(PAIR_VERSION) {
        return None;
    }
    let relay = relay?;
    let code = code?;
    if relay.is_empty() || code.is_empty() {
        return None;
    }
    if !(relay.starts_with("ws://") || relay.starts_with("wss://")) {
        return None;
    }
    Some((relay, code))
}

// <tests from Step 1 go here>
```

Add `pub mod pair_uri;` to `lib.rs` (near `pub mod pairing;`), and to `Cargo.toml`:

```toml
url.workspace = true
form_urlencoded = "1"
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib pair_uri
```
Expected: PASS (3 tests).

- [ ] **Step 5: Format + commit**

```bash
rustfmt --edition 2024 --config skip_children=true yi-agent-rs/crates/yi-agent-app-server/src/pair_uri.rs
git add yi-agent-rs/crates/yi-agent-app-server/src/pair_uri.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs yi-agent-rs/crates/yi-agent-app-server/Cargo.toml
git commit -m "feat(app-server): add QR pairing URI codec"
```

---

### Task 2: TS pair-URI codec (`pairUri.ts`)

**Files:**
- Create: `desktop/src/lib/pairUri.ts`
- Test: `desktop/src/lib/pairUri.test.ts`

**Interfaces:**
- Produces:
  - `export function buildPairUri(relay: string, code: string): string`
  - `export function parsePairUri(text: string): { relay: string; code: string } | null`
  - `export const PAIR_SCHEME = "yiagent";` `export const PAIR_HOST = "pair";` `export const PAIR_VERSION = "1";`

- [ ] **Step 1: Write the failing test**

Create `desktop/src/lib/pairUri.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { buildPairUri, parsePairUri } from "./pairUri";

// 与 Rust 侧 `pair_uri.rs` 的用例**逐字节**共用同一组 fixture。
describe("pairUri", () => {
  it("builds the canonical URI", () => {
    expect(buildPairUri("wss://relay.example.com/ws?session=abc", "ABCD-EFGH")).toBe(
      "yiagent://pair?v=1&relay=wss%3A%2F%2Frelay.example.com%2Fws%3Fsession%3Dabc&code=ABCD-EFGH",
    );
    expect(buildPairUri("ws://192.168.1.5:8080/ws", "WXYZ-1234")).toBe(
      "yiagent://pair?v=1&relay=ws%3A%2F%2F192.168.1.5%3A8080%2Fws&code=WXYZ-1234",
    );
    expect(buildPairUri("wss://r/a b.c?x=1&y=2", "A-B")).toBe(
      "yiagent://pair?v=1&relay=wss%3A%2F%2Fr%2Fa+b.c%3Fx%3D1%26y%3D2&code=A-B",
    );
  });

  it("round-trips", () => {
    for (const [relay, code] of [
      ["wss://relay.example.com/ws?session=abc", "ABCD-EFGH"],
      ["ws://192.168.1.5:8080/ws", "WXYZ-1234"],
    ]) {
      expect(parsePairUri(buildPairUri(relay, code))).toEqual({ relay, code });
    }
  });

  it("rejects malformed payloads", () => {
    expect(parsePairUri("hello")).toBeNull();
    expect(parsePairUri("https://pair?v=1&relay=wss%3A%2F%2Fr&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=2&relay=wss%3A%2F%2Fr&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&relay=wss%3A%2F%2Fr")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&relay=http%3A%2F%2Fr&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&relay=&code=C")).toBeNull();
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd desktop && npx vitest run src/lib/pairUri.test.ts
```
Expected: FAIL — cannot resolve `./pairUri`.

- [ ] **Step 3: Write minimal implementation**

Create `desktop/src/lib/pairUri.ts`:

```ts
/**
 * 二维码配对载荷编解码（S4）。
 *
 * 契约与 Rust 侧 `yi-agent-app-server/src/pair_uri.rs` 完全一致：
 * `yiagent://pair?v=1&relay=<percent-encoded relay url>&code=<code>`。
 * 两份实现用同一组 fixture 断言，防止格式漂移。纯函数、无依赖。
 *
 * `relay` 可装中继地址或局域网直连地址；因自身含 `?`，作 query 值须 form-encode。
 */

export const PAIR_SCHEME = "yiagent";
export const PAIR_HOST = "pair";
export const PAIR_VERSION = "1";

/** 按契约拼出二维码文本（form 编码，与 Rust `form_urlencoded` 一致）。 */
export function buildPairUri(relay: string, code: string): string {
  const p = new URLSearchParams();
  p.set("v", PAIR_VERSION);
  p.set("relay", relay);
  p.set("code", code);
  return `${PAIR_SCHEME}://${PAIR_HOST}?${p.toString()}`;
}

/** 解析二维码文本；不满足契约（scheme/host/版本/字段/ws 前缀任一不符）返回 null。 */
export function parsePairUri(text: string): { relay: string; code: string } | null {
  let u: URL;
  try {
    u = new URL(text);
  } catch {
    return null;
  }
  if (u.protocol !== `${PAIR_SCHEME}:` || u.hostname !== PAIR_HOST) return null;
  if (u.searchParams.get("v") !== PAIR_VERSION) return null;
  const relay = u.searchParams.get("relay") ?? "";
  const code = u.searchParams.get("code") ?? "";
  if (relay.length === 0 || code.length === 0) return null;
  if (!(relay.startsWith("ws://") || relay.startsWith("wss://"))) return null;
  return { relay, code };
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd desktop && npx vitest run src/lib/pairUri.test.ts
```
Expected: PASS (3 tests). If the space fixture case fails, it means JS encoded differently — do NOT change the Rust side; stop and report.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/pairUri.ts desktop/src/lib/pairUri.test.ts
git commit -m "feat(desktop): add QR pairing URI codec"
```

---

### Task 3: Desktop settings page renders the QR code

**Files:**
- Modify: `desktop/package.json` (add `qrcode` dep + `@types/qrcode` dev dep)
- Modify: `desktop/src/components/SettingsRemoteTab.tsx`
- Test: `desktop/src/components/SettingsRemoteTab.test.tsx`

**Interfaces:**
- Consumes: `buildPairUri` from `desktop/src/lib/pairUri.ts`.
- Produces: the settings page renders an element with `aria-label="配对二维码"` when a code exists and `relayUrl` is non-empty; no such element when `relayUrl` is empty.

- [ ] **Step 1: Install the dependency**

```bash
cd desktop && npm install qrcode@1.5.4 && npm install -D @types/qrcode
```

- [ ] **Step 2: Write the failing test**

Add to `desktop/src/components/SettingsRemoteTab.test.tsx` (follow the file's existing render/`call` mock style — it already mints via a fake `call`; mirror that setup):

```tsx
it("renders a QR code once a code is minted and a relay url is present", async () => {
  const call = vi.fn(async (method: string) =>
    method === "pair/create"
      ? { code: "ABCD-EFGH", expires_in: 300 }
      : { devices: [] },
  );
  render(<SettingsRemoteTab call={call} initialRelayUrl="wss://relay.example.com/ws?session=abc" />);
  fireEvent.click(screen.getByRole("button", { name: /生成配对码/ }));
  await waitFor(() => expect(screen.getByLabelText("配对二维码")).toBeTruthy());
});

it("omits the QR code when no relay url is entered", async () => {
  const call = vi.fn(async (method: string) =>
    method === "pair/create" ? { code: "ABCD-EFGH", expires_in: 300 } : { devices: [] },
  );
  render(<SettingsRemoteTab call={call} />);
  fireEvent.click(screen.getByRole("button", { name: /生成配对码/ }));
  await waitFor(() => expect(screen.getByLabelText("配对码")).toBeTruthy());
  expect(screen.queryByLabelText("配对二维码")).toBeNull();
});
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cd desktop && npx vitest run src/components/SettingsRemoteTab.test.tsx
```
Expected: FAIL — `Unable to find a label with the text of: 配对二维码`.

- [ ] **Step 4: Implement rendering**

In `SettingsRemoteTab.tsx`:

1. Add imports:
```tsx
import QRCode from "qrcode";
import { buildPairUri } from "../lib/pairUri";
```

2. Add state and a generation effect that keys off `code` + `relayUrl`:
```tsx
const [qrSvg, setQrSvg] = useState<string | null>(null);

useEffect(() => {
  let cancelled = false;
  if (code === null || relayUrl.trim().length === 0) {
    setQrSvg(null);
    return;
  }
  QRCode.toString(buildPairUri(relayUrl.trim(), code), { type: "svg", margin: 1 })
    .then((svg) => {
      if (!cancelled) setQrSvg(svg);
    })
    .catch(() => {
      if (!cancelled) setQrSvg(null);
    });
  return () => {
    cancelled = true;
  };
}, [code, relayUrl]);
```

3. Render it next to the existing code chip (inside the `code !== null` block):
```tsx
{qrSvg !== null && (
  <div
    aria-label="配对二维码"
    className="mt-3 h-40 w-40 [&>svg]:h-full [&>svg]:w-full"
    dangerouslySetInnerHTML={{ __html: qrSvg }}
  />
)}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd desktop && npx vitest run src/components/SettingsRemoteTab.test.tsx && npx tsc --noEmit
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add desktop/package.json desktop/package-lock.json desktop/src/components/SettingsRemoteTab.tsx desktop/src/components/SettingsRemoteTab.test.tsx
git commit -m "feat(desktop): show a pairing QR code in the remote-access settings"
```

---

### Task 4: CLI `pair code --relay` prints a QR code

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml` (add `qrcode = "0.14"`)
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs` (`PairAction::Code` gains `relay`)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (`run_pair` + new pure formatter)

**Interfaces:**
- Consumes: `yi_agent_app_server::pair_uri::build_pair_uri`.
- Produces: `fn format_pair_code_with_qr(code: &str, expires_in: u64, relay: Option<&str>) -> String` (pure).

- [ ] **Step 1: Write the failing test**

In `main.rs`'s test module (follow the existing `format_pair_code` tests), add:

```rust
#[test]
fn pair_code_without_relay_is_unchanged() {
    assert_eq!(format_pair_code_with_qr("ABCD-EFGH", 300, None), "ABCD-EFGH (valid 300s)\n");
}

#[test]
fn pair_code_with_relay_appends_a_qr_block() {
    let out = format_pair_code_with_qr("ABCD-EFGH", 300, Some("wss://r/ws?session=x"));
    assert!(out.starts_with("ABCD-EFGH (valid 300s)\n"));
    // Unicode 半块字符是 QR 渲染的标志。
    assert!(out.contains('█') || out.contains('▀') || out.contains('▄'));
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib pair_code_with_
```
Expected: FAIL — `cannot find function format_pair_code_with_qr`.

- [ ] **Step 3: Implement**

In `config.rs`, change the variant:
```rust
    /// Mint a one-time pairing code and print it (default).
    Code {
        /// Relay URL to embed in the printed QR code (optional). When omitted,
        /// output is unchanged (text code only).
        #[arg(long)]
        relay: Option<String>,
    },
```

In `main.rs`, add the formatter and use it in `run_pair`:
```rust
/// 文本码，外加（给了 `relay` 时）一个 Unicode 二维码。纯函数，便于单测。
fn format_pair_code_with_qr(code: &str, expires_in: u64, relay: Option<&str>) -> String {
    let mut out = format!("{}\n", format_pair_code(code, expires_in));
    if let Some(relay) = relay {
        let uri = yi_agent_app_server::pair_uri::build_pair_uri(relay, code);
        if let Ok(qr) = qrcode::QrCode::new(uri.as_bytes()) {
            let art = qr
                .render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build();
            out.push_str(&art);
            out.push('\n');
        }
    }
    out
}
```

Update the `PairAction::Code` arm:
```rust
        PairAction::Code { relay } => {
            let code = pairing.create_code();
            print!(
                "{}",
                format_pair_code_with_qr(&code.code, code.expires_in, relay.as_deref())
            );
        }
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib pair_code
```
Expected: PASS.

- [ ] **Step 5: Format + commit**

```bash
rustfmt --edition 2024 --config skip_children=true yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/config.rs
git add yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(cli): print a pairing QR code with pair code --relay"
```

---

### Task 5: Camera scan loop (`qrScanner.ts`)

**Files:**
- Modify: `desktop/package.json` (add `jsqr@1.4.0`)
- Create: `desktop/src/lib/qrScanner.ts`
- Test: `desktop/src/lib/qrScanner.test.ts`

**Interfaces:**
- Produces:
  - `export interface ScanDeps { getUserMedia?: (c: MediaStreamConstraints) => Promise<MediaStream>; grabFrame?: (video: HTMLVideoElement) => ImageData | null; decode?: (data: ImageData) => string | null; intervalMs?: number; }`
  - `export function startCameraScan(video: HTMLVideoElement, onResult: (text: string) => void, onError: (e: unknown) => void, deps?: ScanDeps): () => void` — returns a `stop()` function.
  - `export const SCAN_INTERVAL_MS = 200;`

- [ ] **Step 1: Install the dependency**

```bash
cd desktop && npm install jsqr@1.4.0
```

- [ ] **Step 2: Write the failing test**

Create `desktop/src/lib/qrScanner.test.ts`:

```ts
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { startCameraScan } from "./qrScanner";

function fakeVideo(): HTMLVideoElement {
  return { srcObject: null, play: vi.fn(async () => {}) } as unknown as HTMLVideoElement;
}
function fakeStream(): MediaStream {
  const stop = vi.fn();
  return { getTracks: () => [{ stop }] } as unknown as MediaStream;
}
const frame = ((): ImageData => ({}) as ImageData);

describe("startCameraScan", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("stops and reports on the first successful decode", async () => {
    const onResult = vi.fn();
    const decode = vi.fn().mockReturnValue("yiagent://pair?x");
    const grabFrame = vi.fn().mockReturnValue(frame);
    const stop = startCameraScan(fakeVideo(), onResult, vi.fn(), {
      getUserMedia: async () => fakeStream(),
      grabFrame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(250);
    expect(onResult).toHaveBeenCalledWith("yiagent://pair?x");
    const callsAfterHit = decode.mock.calls.length;
    await vi.advanceTimersByTimeAsync(600);
    expect(decode.mock.calls.length).toBe(callsAfterHit); // 命中即停
    stop();
  });

  it("keeps scanning until a frame decodes", async () => {
    const onResult = vi.fn();
    let n = 0;
    const decode = vi.fn(() => (++n >= 3 ? "hit" : null));
    startCameraScan(fakeVideo(), onResult, vi.fn(), {
      getUserMedia: async () => fakeStream(),
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(700);
    expect(onResult).toHaveBeenCalledWith("hit");
  });

  it("keeps scanning when the decoder throws", async () => {
    const onResult = vi.fn();
    let n = 0;
    const decode = vi.fn(() => {
      if (++n === 1) throw new Error("boom");
      return n >= 3 ? "hit" : null;
    });
    startCameraScan(fakeVideo(), onResult, vi.fn(), {
      getUserMedia: async () => fakeStream(),
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(700);
    expect(onResult).toHaveBeenCalledWith("hit");
  });

  it("reports an error and does not loop when getUserMedia fails", async () => {
    const onError = vi.fn();
    const decode = vi.fn();
    startCameraScan(fakeVideo(), vi.fn(), onError, {
      getUserMedia: async () => {
        throw new Error("denied");
      },
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(600);
    expect(onError).toHaveBeenCalled();
    expect(decode).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cd desktop && npx vitest run src/lib/qrScanner.test.ts
```
Expected: FAIL — cannot resolve `./qrScanner`.

- [ ] **Step 4: Implement**

Create `desktop/src/lib/qrScanner.ts`:

```ts
/**
 * iOS 配对页的相机扫码循环（S4）。
 *
 * 逻辑与 DOM 解耦：整个过程依赖三件事——拿视频流、抓一帧、解码一帧——都可由
 * `deps` 注入。于是「命中即停」「多帧才命中」「解码抛错继续」「无相机即报错」
 * 都能在 jsdom 里用假实现测。真实 `getUserMedia` 只能真机手验。
 */

import jsQR from "jsqr";

/** 默认逐帧间隔（毫秒）。 */
export const SCAN_INTERVAL_MS = 200;

export interface ScanDeps {
  /** 默认 `navigator.mediaDevices.getUserMedia`。 */
  getUserMedia?: (c: MediaStreamConstraints) => Promise<MediaStream>;
  /** 默认：把 video 当前帧画到 canvas 并取 `ImageData`。 */
  grabFrame?: (video: HTMLVideoElement) => ImageData | null;
  /** 默认：`jsQR` 包装。返回解码文本或 null。 */
  decode?: (data: ImageData) => string | null;
  intervalMs?: number;
}

/** 默认抓帧：canvas 尺寸随视频尺寸，取整帧像素。 */
function defaultGrabFrame(video: HTMLVideoElement): ImageData | null {
  const w = video.videoWidth;
  const h = video.videoHeight;
  if (!w || !h) return null;
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  ctx.drawImage(video, 0, 0, w, h);
  return ctx.getImageData(0, 0, w, h);
}

/** 默认解码：`jsQR`（npm `jsqr`，导入名 `jsQR`）。 */
function defaultDecode(data: ImageData): string | null {
  const r = jsQR(data.data, data.width, data.height, { inversionAttempts: "dontInvert" });
  return r?.data ?? null;
}

/**
 * 打开相机并逐帧解码，命中即调 `onResult` 并停止。返回 `stop()` 用于清理。
 * `getUserMedia` 失败时调 `onError` 且不启动循环（表单仍可手输）。
 */
export function startCameraScan(
  video: HTMLVideoElement,
  onResult: (text: string) => void,
  onError: (e: unknown) => void,
  deps: ScanDeps = {},
): () => void {
  const intervalMs = deps.intervalMs ?? SCAN_INTERVAL_MS;
  const grabFrame = deps.grabFrame ?? defaultGrabFrame;
  const decode = deps.decode ?? defaultDecode;
  const getUserMedia =
    deps.getUserMedia ??
    ((c: MediaStreamConstraints) => navigator.mediaDevices.getUserMedia(c));

  let timer: ReturnType<typeof setInterval> | null = null;
  let stopped = false;
  let stream: MediaStream | null = null;

  const stop = () => {
    if (stopped) return;
    stopped = true;
    if (timer !== null) {
      clearInterval(timer);
      timer = null;
    }
    stream?.getTracks().forEach((t) => t.stop());
  };

  getUserMedia({ video: { facingMode: "environment" } })
    .then((s) => {
      if (stopped) {
        s.getTracks().forEach((t) => t.stop());
        return;
      }
      stream = s;
      video.srcObject = s;
      void video.play();
      timer = setInterval(() => {
        if (stopped) return;
        const frame = grabFrame(video);
        if (!frame) return;
        let text: string | null = null;
        try {
          text = decode(frame);
        } catch {
          return; // 单帧解码失败不是致命错误，继续。
        }
        if (text) {
          stop();
          onResult(text);
        }
      }, intervalMs);
    })
    .catch((e) => {
      stop();
      onError(e);
    });

  return stop;
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd desktop && npx vitest run src/lib/qrScanner.test.ts && npx tsc --noEmit
```
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add desktop/package.json desktop/package-lock.json desktop/src/lib/qrScanner.ts desktop/src/lib/qrScanner.test.ts
git commit -m "feat(desktop): add a camera QR scan loop"
```

---

### Task 6: Wire the scan button into the pairing screen

**Files:**
- Create: `desktop/src/components/QrScanner.tsx`
- Modify: `desktop/src/components/PairingScreen.tsx`
- Create: `desktop/src-tauri/Info.ios.plist` (camera usage description; Tauri auto-merges it)
- Test: `desktop/src/components/PairingScreen.test.tsx`

**Interfaces:**
- Consumes: `parsePairUri` (Task 2), `startCameraScan` (Task 5).
- Produces: `PairingScreenProps` gains `enableScan?: boolean` (default `isIos()`); props for injection: `scan?: (onText: (t: string) => void, onError: (e: unknown) => void) => () => void`.

- [ ] **Step 1: Add the iOS camera permission file**

Create `desktop/src-tauri/Info.ios.plist` (Tauri merges `Info.plist`/`Info.ios.plist` found next to the Tauri config with the generated one, so this survives `tauri ios init`):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSCameraUsageDescription</key>
	<string>用于扫描桌面端显示的配对二维码</string>
</dict>
</plist>
```

- [ ] **Step 2: Write the failing test**

Add to `desktop/src/components/PairingScreen.test.tsx`:

```tsx
it("auto-pairs when the scanner delivers a valid payload", async () => {
  const redeem = vi.fn(async () => ({ device_id: "d", token: "yia_t", scope: "control" }));
  const storage = { getItem: () => null, setItem: vi.fn(), removeItem: () => {} };
  // scan: 立刻回调一个合法载荷，并返回一个 stop()。
  const scan = (onText: (t: string) => void) => {
    onText("yiagent://pair?v=1&relay=wss%3A%2F%2Fr%2Fws&code=ABCD-EFGH");
    return () => {};
  };
  render(<PairingScreen enableScan redeem={redeem} storage={storage} scan={scan} />);
  fireEvent.click(screen.getByRole("button", { name: /扫码/ }));
  await waitFor(() =>
    expect(redeem).toHaveBeenCalledWith("wss://r/ws", "ABCD-EFGH", expect.any(String)),
  );
});

it("shows a message and does not redeem when the payload is not a pairing QR", async () => {
  const redeem = vi.fn();
  const scan = (onText: (t: string) => void) => {
    onText("https://example.com/not-a-pairing-code");
    return () => {};
  };
  render(<PairingScreen enableScan redeem={redeem} scan={scan} />);
  fireEvent.click(screen.getByRole("button", { name: /扫码/ }));
  await waitFor(() =>
    expect(screen.getByText("不是有效的配对二维码")).toBeTruthy(),
  );
  expect(redeem).not.toHaveBeenCalled();
});

it("falls back to manual entry when the camera is unavailable", async () => {
  const scan = (_onText: (t: string) => void, onError: (e: unknown) => void) => {
    onError(new Error("no camera"));
    return () => {};
  };
  render(<PairingScreen enableScan scan={scan} />);
  fireEvent.click(screen.getByRole("button", { name: /扫码/ }));
  await waitFor(() =>
    expect(screen.getByText("无法访问相机，可手输配对码")).toBeTruthy(),
  );
  expect(screen.getByLabelText("配对码")).toBeTruthy(); // 表单仍可用
});
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cd desktop && npx vitest run src/components/PairingScreen.test.tsx
```
Expected: FAIL — no button named `/扫码/`, unknown prop `scan`.

- [ ] **Step 4: Implement `QrScanner.tsx`**

Create `desktop/src/components/QrScanner.tsx`:

```tsx
import { useEffect, useRef } from "react";
import { startCameraScan, type ScanDeps } from "../lib/qrScanner";

/** 全屏相机预览 + 扫描循环的薄 UI。逻辑全在 `lib/qrScanner`。 */
export interface QrScannerProps {
  onText: (text: string) => void;
  onError: (e: unknown) => void;
  /** 注入接缝（测试用）；生产省略。 */
  deps?: ScanDeps;
}

export function QrScanner({ onText, onError, deps }: QrScannerProps) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    const stop = startCameraScan(video, onText, onError, deps);
    return stop;
  }, [onText, onError, deps]);
  return <video ref={videoRef} aria-label="相机预览" playsInline className="h-full w-full object-cover" />;
}
```

- [ ] **Step 5: Wire `PairingScreen.tsx`**

Add the scan seam and state, and render the button + overlay:

```tsx
import { parsePairUri } from "../lib/pairUri";
import { QrScanner } from "./QrScanner";
import { isIos } from "../lib/platform";

/** 扫码结果不是合法配对载荷时的文案。 */
export const NOT_A_PAIRING_QR_TEXT = "不是有效的配对二维码";
/** 相机不可用（拒绝/无设备）时的文案；表单仍可手输。 */
export const CAMERA_UNAVAILABLE_TEXT = "无法访问相机，可手输配对码";
```

Extend props:
```tsx
  /** 是否显示「扫码」按钮；默认仅 iOS。 */
  enableScan?: boolean;
  /** 扫码接缝（测试用）；默认 `undefined` → 用内建 `QrScanner`。 */
  scan?: (onText: (t: string) => void, onError: (e: unknown) => void) => () => void;
```

Add to the component:
```tsx
  const [scanning, setScanning] = useState(false);
  const canScan = enableScan ?? isIos();

  const runRedeem = async (u: string, c: string) => {
    const name = deviceName.trim() || inferDeviceName();
    setBusy(true);
    setError(null);
    try {
      if (onSubmit) await onSubmit(u, c, name);
      else {
        const device = await (redeem ?? defaultRedeem)(u, c, name);
        saveRemoteConfig(storage ?? defaultStorage(), { url: u, token: device.token });
      }
      onPaired?.();
    } catch (err) {
      setError(isInvalidCode(err) ? INVALID_CODE_TEXT : CONNECT_FAILED_TEXT);
    } finally {
      setBusy(false);
      setScanning(false);
    }
  };

  const handleScanText = (text: string) => {
    const parsed = parsePairUri(text);
    if (!parsed) {
      setError(NOT_A_PAIRING_QR_TEXT);
      return; // 扫描器保持打开，可重试
    }
    setUrl(parsed.relay);
    setCode(parsed.code);
    void runRedeem(parsed.relay, parsed.code); // 自动配对
  };
```

Refactor `submit` to call `runRedeem(trimmedUrl, trimmedCode)`, and add the button + overlay to the JSX:
```tsx
        {canScan && (
          <button
            type="button"
            onClick={() => {
              setError(null);
              setScanning(true);
            }}
            className="mt-3 w-full rounded-md border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg"
          >
            扫码
          </button>
        )}
        {scanning && (
          <div className="fixed inset-0 z-50 bg-black">
            {scan ? (
              <ScanHost scan={scan} onText={handleScanText} onError={() => { setError(CAMERA_UNAVAILABLE_TEXT); setScanning(false); }} />
            ) : (
              <QrScanner onText={handleScanText} onError={() => { setError(CAMERA_UNAVAILABLE_TEXT); setScanning(false); }} />
            )}
          </div>
        )}
```

Add a tiny adapter so an injected `scan` keeps a stable identity (avoid re-running the effect every render):
```tsx
function ScanHost({ scan, onText, onError }: {
  scan: NonNullable<PairingScreenProps["scan"]>;
  onText: (t: string) => void;
  onError: (e: unknown) => void;
}) {
  const ref = useRef({ onText, onError });
  ref.current = { onText, onError };
  useEffect(() => scan((t) => ref.current.onText(t), (e) => ref.current.onError(e)), [scan]);
  return null;
}
```

- [ ] **Step 6: Run tests to verify they pass**

```bash
cd desktop && npx vitest run src/components/PairingScreen.test.tsx && npx vitest run && npx tsc --noEmit
```
Expected: PASS (new 3 + all existing).

- [ ] **Step 7: Commit**

```bash
git add desktop/src/components/QrScanner.tsx desktop/src/components/PairingScreen.tsx desktop/src/components/PairingScreen.test.tsx desktop/src-tauri/Info.ios.plist
git commit -m "feat(desktop): scan a pairing QR code in the iOS pairing screen"
```

---

### Task 7: Document the new pairing path

**Files:**
- Modify: `docs/relay-deploy.md` (§六 known-limitations: remove "无二维码扫描"; §4.4 entry points)
- Modify: `README.md`, `README.en.md`

- [ ] **Step 1: Update `docs/relay-deploy.md`**

- In §六, delete the bullet "**二维码扫描**——目前是文本码手输（配对本身已可用，含经中继的帧级兑换）。" and the two "无二维码扫描" mentions in the file header (§ line 9) and §4.4 note.
- In §4.4, add: desktop 「远程访问」页在填了中继地址时显示二维码，`yi-agent pair code --relay <url>` 在终端打印二维码；iOS 配对页「扫码」按钮用相机扫码后自动配对。文本手输路径保留。

- [ ] **Step 2: Update both READMEs**

Add one sentence to the iOS/remote section of `README.md` and `README.en.md`: pairing can be done by scanning a QR code shown by the desktop (settings page) or CLI (`yi-agent pair code --relay`); the code carries the relay URL + one-time pairing code, not a token.

- [ ] **Step 3: Commit**

```bash
git add docs/relay-deploy.md README.md README.en.md
git commit -m "docs: describe QR-code pairing"
```

---

## Self-Review

**Spec coverage:**
- G1 unified payload → Task 1 + Task 2 (shared fixtures). ✓
- G2 desktop renders QR → Task 3. ✓
- G3 CLI renders QR → Task 4. ✓
- G4 in-app scan → Task 5 + Task 6. ✓
- G5 zero regression → Task 4 test (no-relay unchanged), Task 6 test (camera failure keeps form). ✓
- §5 error table → Task 6 tests (invalid payload, camera unavailable) + reused `INVALID_CODE_TEXT`. ✓
- §7 test strategy → covered across tasks; gates listed in Global Constraints. ✓
- §8 no protocol change → no RPC touched. ✓

**Placeholder scan:** none — every step has concrete code/commands. The one spec item left open (iOS plist injection) is resolved to `desktop/src-tauri/Info.ios.plist` (Tauri merge behavior verified against `@tauri-apps/cli/config.schema.json`).

**Type consistency:** `buildPairUri`/`parsePairUri` (TS) and `build_pair_uri`/`parse_pair_uri` (Rust) used consistently; `startCameraScan` signature identical in Task 5 and Task 6; `ScanDeps` shared.
