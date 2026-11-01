/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { SlashPopup } from "./SlashPopup";
import { SLASH_COMMANDS } from "../lib/slash";

afterEach(cleanup);

describe("SlashPopup", () => {
  it("renders nothing when there are no matches", () => {
    const { container } = render(<SlashPopup commands={[]} selected={0} />);
    // The brief specifies `toBeEmptyDOMElement()`, but
    // `@testing-library/jest-dom` is not a dependency of this workspace
    // (no test imports it and it does not resolve from node_modules), so this
    // asserts the identical property with a built-in matcher: a `null` render
    // leaves the container with no child nodes at all.
    expect(container.childNodes).toHaveLength(0);
  });

  it("renders every command with its usage and description", () => {
    // The brief passes `selected={0}` here while asserting below that the marked
    // option is `/help`, and the next test pins the semantics as index-based
    // (`selected={2}` -> `/config`). Those cannot both hold, so the brief's `0`
    // is a typo; the index below selects the very item the assertions name.
    // 目录加入 `/fork` 后整体后移一位：/help 由 4 变 5，/config 由 2 变 3。
    const helpIndex = SLASH_COMMANDS.findIndex((c) => c.name === "help");
    render(<SlashPopup commands={SLASH_COMMANDS} selected={helpIndex} />);
    expect(screen.getAllByTestId("slash-option")).toHaveLength(SLASH_COMMANDS.length);
    const selected = screen
      .getAllByTestId("slash-option")
      .filter((el) => el.getAttribute("data-selected") === "true");
    expect(selected).toHaveLength(1);
    expect(selected[0].textContent).toContain("/help [command]");
    expect(selected[0].textContent).toContain("显示帮助信息");
  });

  it("marks exactly the selected option", () => {
    const configIndex = SLASH_COMMANDS.findIndex((c) => c.name === "config");
    render(<SlashPopup commands={SLASH_COMMANDS} selected={configIndex} />);
    const selected = screen
      .getAllByTestId("slash-option")
      .filter((el) => el.getAttribute("data-selected") === "true");
    expect(selected).toHaveLength(1);
    expect(selected[0].textContent).toContain("/config");
  });
});
