/**
 * The plugin's boundary, enforced structurally.
 *
 * This package is built and shipped on its own: the host app is never
 * recompiled to change the board, and the published bundle must work on a
 * machine that has no copy of this repository. Both promises break silently if
 * a file starts importing the host's source tree, so the imports are checked
 * here rather than trusted to review.
 *
 * Reading the files (instead of importing them) is deliberate: an import-based
 * check would fail the moment a violation is introduced, which is fine, but it
 * would report a module-resolution error rather than naming the rule that was
 * broken. A text scan names the file and the forbidden specifier.
 */

import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import manifest from "../../plugin.json";
import { METHODS, WRITE_METHODS } from "./lib/methods";

const UI_ROOT = resolve(process.cwd(), "src");

/** Every `.ts`/`.tsx` file under `src`, excluding this check itself. */
function sourceFiles(dir: string = UI_ROOT): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      out.push(...sourceFiles(path));
      continue;
    }
    if (!/\.(ts|tsx)$/.test(entry)) continue;
    if (path === resolve(UI_ROOT, "boundary.test.ts")) continue;
    out.push(path);
  }
  return out;
}

/** Bare and relative import specifiers, from both `import` and `require`. */
function specifiersOf(source: string): string[] {
  const found: string[] = [];
  const patterns = [
    /(?:^|\n)\s*import\s[^;]*?from\s+["']([^"']+)["']/g,
    /(?:^|\n)\s*import\s+["']([^"']+)["']/g,
    /\brequire\(\s*["']([^"']+)["']\s*\)/g,
    /\bimport\(\s*["']([^"']+)["']\s*\)/g,
  ];
  for (const pattern of patterns) {
    for (const match of source.matchAll(pattern)) found.push(match[1]);
  }
  return found;
}

interface Rule {
  /** Human-readable name, used in the failure message. */
  what: string;
  /** Returns true when the specifier violates the rule. */
  violates: (specifier: string) => boolean;
}

const RULES: Rule[] = [
  {
    what: "the host desktop source tree",
    // `../desktop/...`, `../../desktop/...`, an absolute path into it, or a path
    // that climbs out of the package at all: the bundle ships alone, so anything
    // outside it does not exist on the user's machine.
    violates: (s) =>
      s.includes("desktop/src") ||
      /^(\.\.\/){2,}/.test(s) ||
      s.startsWith("/"),
  },
  {
    what: "a Tauri API",
    // The page runs in a plain WebView under a bridge. Reaching for Tauri would
    // work in the desktop shell and fail everywhere else, which is exactly the
    // coupling this migration removes.
    violates: (s) => s === "@tauri-apps/api" || s.startsWith("@tauri-apps/"),
  },
  {
    what: "the host's private RPC client or protocol types",
    violates: (s) => /(^|\/)(lib\/)?(rpc|rpcCall|routes|protocol)(\.ts)?$/.test(s) && s.startsWith("."),
  },
  {
    what: "the host's private error or popover modules",
    violates: (s) =>
      /(^|\/)(errorMessage|connectionError|ui\/Popover)$/.test(s.replace(/\.tsx?$/, "")),
  },
];

describe("plugin UI boundary", () => {
  const files = sourceFiles();

  it("scans a non-trivial number of files (so an empty scan cannot pass)", () => {
    expect(files.length).toBeGreaterThan(10);
  });

  for (const rule of RULES) {
    it(`never imports ${rule.what}`, () => {
      const violations: string[] = [];
      for (const file of files) {
        for (const specifier of specifiersOf(readFileSync(file, "utf8"))) {
          if (rule.violates(specifier)) {
            violations.push(`${relative(UI_ROOT, file)} → ${specifier}`);
          }
        }
      }
      expect(violations).toEqual([]);
    });
  }

  it("describes each rule in a way a failure message can use", () => {
    // Guards against someone emptying a rule's predicate to silence it: a rule
    // that matches nothing at all would make its test vacuous.
    expect(RULES.every((rule) => rule.what.length > 0)).toBe(true);
    expect(RULES).toHaveLength(4);
  });

  it("allows the plugin's own relative imports and its SDK dependency", () => {
    // The rules must not be so broad that they forbid the two things the page
    // legitimately needs: its own modules and the published SDK.
    const allowed = ["./lib/board", "../lib/cards", "@yi-agent/plugin-sdk", "react"];
    for (const rule of RULES) {
      for (const specifier of allowed) {
        expect(rule.violates(specifier)).toBe(false);
      }
    }
  });

  it("catches each violation the rules exist for", () => {
    // Positive controls: a rule that cannot fail is worse than no rule, because
    // it looks like protection.
    expect(RULES[0].violates("../../desktop/src/lib/rpc")).toBe(true);
    expect(RULES[0].violates("../../../something-else")).toBe(true);
    expect(RULES[1].violates("@tauri-apps/api/core")).toBe(true);
    expect(RULES[2].violates("../lib/rpc")).toBe(true);
    expect(RULES[3].violates("../lib/errorMessage")).toBe(true);
  });
});

describe("declared methods match the manifest", () => {
  it("declares exactly the methods the UI calls", () => {
    // The host refuses any method the manifest does not declare, so a mismatch
    // is a runtime failure that only shows up on the one code path nobody
    // exercised. Comparing the two lists catches it at build time.
    const declared = manifest.methods.map((method) => method.name).sort();
    const used = Object.values(METHODS).sort();
    expect(used).toEqual(declared);
  });

  it("marks every mutating method as a write in the manifest", () => {
    // A method declared `Read` that actually mutates would be retried freely by
    // the host's read policy — the exact failure the effect declaration exists
    // to prevent.
    const effects = new Map(manifest.methods.map((method) => [method.name, method.effect]));
    for (const method of WRITE_METHODS) {
      expect(effects.get(method)).not.toBe("Read");
      expect(effects.get(method)).not.toBeUndefined();
    }
  });

  it("declares the two views the page serves", () => {
    const kinds = manifest.views.map((view) => view.kind).sort();
    expect(kinds).toEqual(["main", "settings"]);
  });

  it("names a program entry or none, but never a test root", () => {
    // `plugin.json` is JSON, so TS narrows `program` to `null` from the literal.
    // The assertion has to describe both shapes the manifest may take across
    // T22 (view-only today, program-backed afterwards), not just today's.
    const program = manifest.program as { entry: string } | null;
    if (program !== null) {
      expect(program.entry.endsWith("superpowers-kanban")).toBe(true);
    }
  });
});
