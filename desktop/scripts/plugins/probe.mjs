// CLI orchestrator for the isolated-plugin-WebView probe.
//
// This file only orchestrates: it builds nothing itself and decides nothing
// about what counts as a pass. Its job is to launch the **real** bundle built
// from this worktree inside a throwaway instance root, wait for the native side
// to atomically write the report, and then hand that report to `probe-lib.mjs`
// for judgement.
//
// Two rules are load-bearing:
//
//   * `open -a` is forbidden. It hands the launch to LaunchServices, which may
//     resolve a *different* installed copy and start the user's real sidecar.
//     We exec `Contents/MacOS/desktop` ourselves so the binary under test is
//     provably the one we hashed.
//   * A timeout is a failure, never a recovery. If the app had to be killed,
//     it did not recover from a blocked page; recording that as a pass would
//     invert the entire point of the `blocked_script_recovery` case.

import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { checkProbeReport, PROBE_CASES } from "./probe-lib.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DESKTOP_DIR = path.resolve(HERE, "..", "..");
const REPO_ROOT = path.resolve(DESKTOP_DIR, "..");
const MANIFEST = path.join(DESKTOP_DIR, "src-tauri", "Cargo.toml");

const CONFIG_PATH = path.join(DESKTOP_DIR, "src-tauri", "tauri.conf.json");
const DEFAULT_SUITE = "isolation";

/** Hard ceiling for one whole run. The probe is build-free, so this is generous. */
const OVERALL_TIMEOUT_MS = 240_000;
/** How long to wait for the report file after the app exits. */
const REPORT_GRACE_MS = 5_000;
/** How long a killed app gets to die before SIGKILL. */
const KILL_GRACE_MS = 5_000;

function parseArgs(argv) {
  let suite = DEFAULT_SUITE;
  let output = null;
  let fixture = null;
  let keep = false;

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case "--suite":
        suite = argv[++index] ?? "";
        break;
      case "--output":
        output = argv[++index] ?? null;
        break;
      case "--fixture-root":
        fixture = argv[++index] ?? null;
        break;
      case "--keep-instance":
        keep = true;
        break;
      case "--help":
        printUsage();
        process.exit(0);
        break;
      default:
        throw new Error(`unknown argument \`${arg}\``);
    }
  }

  if (!output) throw new Error("--output <path> is required");
  if (!suite) throw new Error("--suite needs a non-empty value");

  return {
    suite,
    output: path.resolve(REPO_ROOT, output),
    fixture: path.resolve(
      REPO_ROOT,
      fixture ?? path.join("plugins", "fixtures", "example-counter", "probe"),
    ),
    keep,
  };
}

function printUsage() {
  process.stdout.write(
    [
      "usage: node desktop/scripts/plugins/probe.mjs --output <file> [options]",
      "",
      "  --output <path>        where the native report is written (required)",
      "  --suite <name>         probe suite to run (default: isolation)",
      "  --fixture-root <path>  plugin fixture to serve (default: example-counter/probe)",
      "  --keep-instance        keep the temporary instance root for inspection",
      "",
    ].join("\n"),
  );
}

/**
 * Resolve the Cargo target directory from Cargo itself.
 *
 * Hard-coding `desktop/src-tauri/target` would silently point at a stale build
 * the day someone sets `CARGO_TARGET_DIR`, so ask the manifest.
 */
function targetDirectory() {
  const raw = execFileSync(
    "cargo",
    ["metadata", "--manifest-path", MANIFEST, "--no-deps", "--format-version", "1"],
    { cwd: REPO_ROOT, encoding: "utf8", maxBuffer: 32 * 1024 * 1024 },
  );
  const metadata = JSON.parse(raw);
  if (!metadata.target_directory) {
    throw new Error("cargo metadata did not report a target_directory");
  }
  return metadata.target_directory;
}

function sha256File(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

/**
 * A sha256 over every file in the bundle, so the report names its subject.
 *
 * Directory entries are sorted so the digest is stable across runs and
 * filesystems; `readdirSync` order is not guaranteed.
 */
function sha256Tree(root) {
  const hash = createHash("sha256");
  const walk = (dir) => {
    for (const name of readdirSync(dir).sort()) {
      const full = path.join(dir, name);
      const stats = statSync(full);
      if (stats.isDirectory()) {
        walk(full);
      } else if (stats.isFile()) {
        hash.update(path.relative(root, full));
        hash.update(`\0${stats.size}\0`);
        hash.update(readFileSync(full));
      }
    }
  };
  walk(root);
  return hash.digest("hex");
}

/**
 * The bundle is named after `productName` in `tauri.conf.json`, *not* after the
 * Cargo package. Reading it keeps the runner honest when the product renames.
 */
function productName() {
  const config = JSON.parse(readFileSync(CONFIG_PATH, "utf8"));
  const name = config.productName;
  if (typeof name !== "string" || name.trim() === "") {
    throw new Error(`${CONFIG_PATH} has no productName`);
  }
  return name;
}

function resolveBundle(targetDirectoryPath) {
  const appName = productName();
  const bundle = path.join(targetDirectoryPath, "release", "bundle", "macos", `${appName}.app`);
  if (!existsSync(bundle)) {
    throw new Error(
      `bundle not found at ${bundle}\n` +
        "build it first:\n" +
        "  (cd desktop && npm ci && npm run sidecar:release && npm run build && " +
        "npm run tauri -- build --bundles app --features plugin-probe)",
    );
  }
  // `CFBundleExecutable` is authoritative and is *not* `productName`: the
  // bundle is `yi-agent.app` but the deshell binary inside it is `desktop`
  // (from the Cargo package), with the sidecar placed beside it as `yi-agent`.
  // Guessing here would exec the sidecar and get `unexpected argument
  // '--plugin-probe-run'`.
  const plist = path.join(bundle, "Contents", "Info.plist");
  const executableName = execFileSync(
    "/usr/libexec/PlistBuddy",
    ["-c", "Print :CFBundleExecutable", plist],
    { encoding: "utf8" },
  ).trim();
  if (!executableName) {
    throw new Error(`${plist} has no CFBundleExecutable`);
  }
  const executable = path.join(bundle, "Contents", "MacOS", executableName);
  if (!existsSync(executable)) {
    throw new Error(`bundle has no executable at ${executable}`);
  }
  return { bundle, executable };
}

/**
 * Normalise an instance root exactly like `desktop/src-tauri/src/bridge.rs`:
 * a path that does not already end in `.yi-agent` gets that suffix.
 *
 * Getting this wrong would point the probe at the developer's real
 * `~/.yi-agent`, so the rule is duplicated here deliberately and printed in the
 * run header for eyeballing.
 */
function instanceHome(root) {
  const trimmed = root.trim();
  if (!trimmed) throw new Error("instance root may not be blank");
  return trimmed.endsWith(".yi-agent") ? trimmed : path.join(trimmed, ".yi-agent");
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function readReportWhenReady(file, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(file)) {
      const raw = readFileSync(file, "utf8");
      if (raw.trim() !== "") return raw;
    }
    await sleep(50);
  }
  return null;
}

/** Kill a detached child's whole process group, escalating to SIGKILL. */
async function killGroup(child) {
  try {
    process.kill(-child.pid, "SIGTERM");
  } catch {
    /* already gone */
  }
  const deadline = Date.now() + KILL_GRACE_MS;
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) break;
    await sleep(50);
  }
  try {
    process.kill(-child.pid, "SIGKILL");
  } catch {
    /* already gone */
  }
}

async function runProbe(options) {
  if (process.platform !== "darwin") {
    // The plan requires a launcher adapter per platform. Inventing coverage we
    // do not have would turn a gap into a false pass.
    process.stderr.write(
      `probe: unsupported platform \`${process.platform}\` — no launcher adapter exists\n`,
    );
    return 2;
  }

  const targetDirectoryPath = targetDirectory();
  const { bundle, executable } = resolveBundle(targetDirectoryPath);
  const executableSha256 = sha256File(executable);
  const bundleSha256 = sha256Tree(bundle);

  if (!existsSync(options.fixture)) {
    throw new Error(`fixture root does not exist: ${options.fixture}`);
  }
  mkdirSync(path.dirname(options.output), { recursive: true });
  if (existsSync(options.output)) rmSync(options.output);

  const scratch = mkdtempSync(path.join(tmpdir(), "yi-agent-plugin-probe-"));
  const root = instanceHome(scratch);
  mkdirSync(root, { recursive: true });

  process.stdout.write(
    [
      `probe: suite=${options.suite}`,
      `probe: bundle=${bundle}`,
      `probe: executable sha256=${executableSha256}`,
      `probe: bundle sha256=${bundleSha256}`,
      `probe: instance root=${root}`,
      `probe: fixture=${options.fixture}`,
      `probe: output=${options.output}`,
      "",
    ].join("\n"),
  );

  let child = null;
  try {
    child = spawn(
      executable,
      [
        "--plugin-probe-run",
        "--instance-root",
        root,
        "--fixture-root",
        options.fixture,
        "--output",
        options.output,
        "--suite",
        options.suite,
      ],
      {
        cwd: REPO_ROOT,
        // A fresh instance root on disk is not enough: `YI_AGENT_HOME` is what
        // the shell *and* the sidecar supervisor read, so both must point at
        // the scratch dir. Nothing from the developer's session may leak in.
        env: {
          ...process.env,
          YI_AGENT_HOME: root,
          // The shell reads `YI_AGENT_RELAY` (a stored setting is only
          // consulted when the env var is blank). `YI_AGENT_RELAY_URL` is not
          // a variable this codebase reads, so setting it would be cargo-cult.
          YI_AGENT_RELAY: "",
        },
        // Own process group: the desktop shell supervises a sidecar child, and
        // we must be able to tear down the whole tree, not just the parent.
        detached: true,
        stdio: ["ignore", "inherit", "pipe"],
      },
    );

    const stderrChunks = [];
    child.stderr.on("data", (chunk) => {
      stderrChunks.push(chunk);
      process.stderr.write(chunk);
    });

    const exitInfo = await exitWithin(child, OVERALL_TIMEOUT_MS);
    if (exitInfo === null) {
      // A timeout cannot be rescued by a report: even a complete-looking report
      // is untrustworthy if the app had to be killed to produce it.
      await killGroup(child);
      throw new Error(
        `probe timed out after ${OVERALL_TIMEOUT_MS}ms and the app had to be killed; ` +
          "a killed app is not a recovered app",
      );
    }
    if (exitInfo.code !== 0) {
      throw new Error(
        `desktop exited with code ${exitInfo.code}` +
          (exitInfo.signal ? ` (signal ${exitInfo.signal})` : ""),
      );
    }

    const raw = await readReportWhenReady(options.output, REPORT_GRACE_MS);
    if (raw === null) {
      throw new Error(`no probe report was written to ${options.output}`);
    }

    let report;
    try {
      report = JSON.parse(raw);
    } catch (error) {
      throw new Error(`probe report is not valid JSON: ${error.message}`);
    }

    // The native side hashes its own executable. A mismatch means this report
    // describes a different build than the bundle we are about to blame.
    if (report.appSha256 && report.appSha256 !== executableSha256) {
      throw new Error(
        `report was produced by a different binary: report=${report.appSha256} ` +
          `bundle=${executableSha256}`,
      );
    }
    if (report.suite !== options.suite) {
      throw new Error(`report suite is \`${report.suite}\`, expected \`${options.suite}\``);
    }

    const verdict = checkProbeReport(report);

    const sidecarPath = options.output.replace(/\.json$/, "") + ".run.json";
    writeFileSync(
      sidecarPath,
      `${JSON.stringify(
        {
          bundleSha256,
          executableSha256,
          report: path.relative(REPO_ROOT, options.output),
          stderr: Buffer.concat(stderrChunks).toString("utf8").slice(0, 64 * 1024),
        },
        null,
        2,
      )}\n`,
    );

    process.stdout.write(
      [
        "",
        `probe: PASS — ${verdict.checked}/${PROBE_CASES.length} cases passed`,
        `probe: run metadata=${path.relative(REPO_ROOT, sidecarPath)}`,
        "",
      ].join("\n"),
    );
    return 0;
  } finally {
    if (child && (child.exitCode === null && child.signalCode === null)) {
      await killGroup(child);
    }
    // Always clean up: a leftover instance root would make the next run look
    // like it inherited state, and a leaked sidecar would keep running.
    if (options.keep) {
      process.stdout.write(`probe: kept instance root at ${scratch}\n`);
    } else {
      rmSync(scratch, { recursive: true, force: true });
    }
  }
}

/** Resolve to `{code, signal}` when the child exits, or `null` on timeout. */
function exitWithin(child, timeoutMs) {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve({ code: child.exitCode, signal: child.signalCode });
  }
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), timeoutMs);
    child.once("exit", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal });
    });
  });
}

try {
  process.exit(await runProbe(parseArgs(process.argv.slice(2))));
} catch (error) {
  process.stderr.write(`probe: FAIL — ${error.message}\n`);
  process.exit(1);
}
