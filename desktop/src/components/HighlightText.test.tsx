/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { HighlightText } from "./HighlightText";

afterEach(cleanup);

describe("HighlightText", () => {
  it("renders the text unchanged when there is no query", () => {
    const { container } = render(<HighlightText text="hello world" />);
    expect(container.textContent).toBe("hello world");
    expect(container.querySelectorAll("mark").length).toBe(0);
  });

  it("marks every occurrence", () => {
    const { container } = render(<HighlightText text="a foo b foo" query="foo" />);
    const hits = container.querySelectorAll("mark.search-hit");
    expect(hits.length).toBe(2);
    expect(hits[0].textContent).toBe("foo");
  });

  it("preserves the surrounding text order", () => {
    const { container } = render(<HighlightText text="xy foo z" query="foo" />);
    expect(container.textContent).toBe("xy foo z");
  });

  it("is case-insensitive but keeps the original casing on screen", () => {
    const { container } = render(<HighlightText text="Foo" query="foo" />);
    expect(container.querySelector("mark")?.textContent).toBe("Foo");
  });
});
