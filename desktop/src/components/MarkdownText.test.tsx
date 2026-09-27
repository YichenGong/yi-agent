/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { MarkdownText } from "./MarkdownText";

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn().mockResolvedValue(undefined),
}));

afterEach(cleanup);

describe("MarkdownText", () => {
  it("renders a fenced code block with highlight classes", () => {
    const { container } = render(<MarkdownText text={"```js\nconst x = 1;\n```"} />);
    const code = container.querySelector("pre code");
    expect(code).not.toBeNull();
    expect(code!.className).toContain("hljs");
  });

  it("renders a GFM table", () => {
    const { container } = render(<MarkdownText text={"| a | b |\n| - | - |\n| 1 | 2 |"} />);
    expect(container.querySelector("table")).not.toBeNull();
  });

  it("does not render raw HTML (XSS guard)", () => {
    const { container } = render(
      <MarkdownText text={"before <script>window.__x = 1</script> after"} />,
    );
    expect(container.querySelector("script")).toBeNull();
  });

  it("opens http(s) links via the opener and prevents default", () => {
    const { container } = render(<MarkdownText text={"[x](https://example.com)"} />);
    const a = container.querySelector("a");
    expect(a).not.toBeNull();
    const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
    a!.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
    expect(openUrl).toHaveBeenCalledWith("https://example.com");
  });

  it("does not render a clickable anchor for non-http(s) links", () => {
    const { container } = render(<MarkdownText text={"[x](javascript:alert(1))"} />);
    expect(container.querySelector("a")).toBeNull();
  });
});
