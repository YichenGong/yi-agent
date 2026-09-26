/** @vitest-environment jsdom */
import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { MarkdownText } from "./MarkdownText";

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
});
