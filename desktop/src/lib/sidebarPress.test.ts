/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { resolveSidebarPressTarget } from "./sidebarPress";

afterEach(() => {
  document.body.innerHTML = "";
});

/** 造一棵与侧栏同构的迷你 DOM：组头 + 一条会话行（含一个按钮）+ 一条组长行。 */
function tree(): {
  header: HTMLElement;
  row: HTMLElement;
  pin: HTMLElement;
  blank: HTMLElement;
  leadText: HTMLElement;
} {
  document.body.innerHTML = `
    <aside>
      <div data-ws-header=""><span id="h-text">projA</span></div>
      <div data-lead-row="" data-thread-row="lead-1"><span id="lead-text">lead</span></div>
      <div data-thread-row="t1"><span id="r-text">alpha</span><button id="pin">pin</button></div>
      <div data-linked-child="/b"><span id="c-text">childB</span></div>
      <div id="blank">footer</div>
    </aside>
  `;
  return {
    header: document.querySelector("[data-ws-header]")!,
    row: document.querySelector("#r-text")!.closest("[data-thread-row]")!,
    pin: document.querySelector("#pin")!,
    blank: document.querySelector("#blank")!,
    leadText: document.querySelector("#lead-text")!,
  };
}

describe("resolveSidebarPressTarget", () => {
  it("resolves a press inside a thread row to that row", () => {
    const { row } = tree();
    expect(resolveSidebarPressTarget(document.querySelector("#r-text"))).toBe(row);
  });

  it("still resolves a press inside a normal thread row to that row", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#r-text"))).toBe(
      document.querySelector("[data-thread-row]:not([data-lead-row])"),
    );
  });

  it("returns null for a press on the lead row (no session menu)", () => {
    const { leadText } = tree();
    expect(resolveSidebarPressTarget(leadText)).toBeNull();
  });

  it("resolves a press inside a workspace header to that header", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#h-text"))).toBe(
      document.querySelector("[data-ws-header]"),
    );
  });

  it("resolves a press inside a linked-child section header to that header", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#c-text"))).toBe(
      document.querySelector("[data-linked-child]"),
    );
  });

  it("returns null for a press on an interactive child (pin / delete button)", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#pin"))).toBeNull();
  });

  it("returns null for a press outside any row or header", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#blank"))).toBeNull();
  });

  it("returns null for a null target", () => {
    expect(resolveSidebarPressTarget(null)).toBeNull();
  });
});
