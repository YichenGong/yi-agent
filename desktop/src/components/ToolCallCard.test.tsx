/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { ToolCallCard } from "./ToolCallCard";
import type { Item } from "../lib/protocol";

type ToolCallItem = Extract<Item, { type: "toolCall" }>;

afterEach(cleanup);

const bash = (input: unknown, over: Partial<ToolCallItem> = {}): ToolCallItem => ({
  type: "toolCall",
  id: "i1",
  call_id: "c1",
  name: "bash",
  input,
  status: "completed",
  ...over,
});

describe("ToolCallCard", () => {
  it("shows the command in the collapsed header", () => {
    render(<ToolCallCard item={bash({ command: "git status --short" })} />);
    expect(screen.getByText("git status --short")).toBeTruthy();
  });

  it("shows the path for a read", () => {
    render(<ToolCallCard item={bash({ path: "src/App.tsx" }, { name: "read" })} />);
    expect(screen.getByText("src/App.tsx")).toBeTruthy();
  });

  it("falls back to the bare tool name for an unknown tool", () => {
    render(<ToolCallCard item={bash(42, { name: "mystery" })} />);
    expect(screen.getByText("mystery")).toBeTruthy();
  });

  it("still renders the full input JSON when expanded", () => {
    const { container } = render(<ToolCallCard item={bash({ command: "ls -la" })} />);
    fireEvent.click(screen.getByRole("button"));
    expect(container.textContent).toContain('"command": "ls -la"');
  });

  it("labels the summary for hover and screen readers", () => {
    render(<ToolCallCard item={bash({ command: "ls -la" })} />);
    expect(screen.getByTitle("ls -la")).toBeTruthy();
  });

  it("shows a placeholder when the tool name is empty", () => {
    render(<ToolCallCard item={bash({ command: "ls -la" }, { name: "" })} />);
    expect(screen.getByText("(unknown tool)")).toBeTruthy();
  });
});
