/** @vitest-environment node */
import { describe, expect, it } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

const dir = String((import.meta as unknown as { dirname: string }).dirname);
const css = () => readFileSync(`${dir}/index.css`, "utf8")
  .replace(/\/\*[\s\S]*?\*\//g, "");
const selector = ".thread-search-input::-webkit-search-cancel-button";

function cancelRules() {
  return [...css().matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .map((match) => ({
      selector: match[1].slice(match[1].lastIndexOf(";") + 1).trim(),
      body: match[2],
    }))
    .filter((rule) => rule.selector.includes("::-webkit-search-cancel-button"));
}

describe("sidebar search clear CSS", () => {
  it("scopes the only native clear override to the sidebar input", () => {
    expect(cancelRules().map((rule) => rule.selector)).toEqual([selector]);
  });

  it("hides the native clear control explicitly", () => {
    const body = cancelRules().find((rule) => rule.selector === selector)?.body ?? "";
    expect(body).toMatch(/(?:^|;)\s*-webkit-appearance:\s*none\s*;/);
    expect(body).toMatch(/(?:^|;)\s*appearance:\s*none\s*;/);
    expect(body).toMatch(/(?:^|;)\s*display:\s*none\s*;/);
  });
});
