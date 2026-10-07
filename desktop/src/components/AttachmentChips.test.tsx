/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { AttachmentChips } from "./AttachmentChips";

afterEach(cleanup);

describe("AttachmentChips", () => {
  it("renders one chip per attachment with its name and size", () => {
    render(
      <AttachmentChips
        attachments={[
          { path: "/tmp/report.pdf", name: "report.pdf", size: 2048 },
          { path: "/tmp/notes.docx", name: "notes.docx", size: 512 },
        ]}
        onRemove={() => {}}
      />,
    );
    // The parent needs a stable handle on the row itself (layout/spacing tests in
    // App), so the test id is part of the contract.
    expect(screen.getByTestId("attachment-chips")).toBeTruthy();
    expect(screen.getByText("report.pdf")).toBeTruthy();
    expect(screen.getByText("notes.docx")).toBeTruthy();
    // Sizes are human-readable, not raw bytes.
    expect(screen.getByText("2 KB")).toBeTruthy();
    expect(screen.getByText("512 B")).toBeTruthy();
  });

  it("reports the removed path", () => {
    const onRemove = vi.fn();
    render(
      <AttachmentChips
        attachments={[
          { path: "/tmp/report.pdf", name: "report.pdf", size: 2048 },
          { path: "/tmp/notes.docx", name: "notes.docx", size: 512 },
        ]}
        onRemove={onRemove}
      />,
    );
    // The remove button names WHICH attachment: two chips must not be ambiguous.
    fireEvent.click(screen.getByRole("button", { name: "移除 notes.docx" }));
    expect(onRemove).toHaveBeenCalledWith("/tmp/notes.docx");
    expect(onRemove).toHaveBeenCalledTimes(1);
  });

  it("renders nothing when there are no attachments", () => {
    const { container } = render(<AttachmentChips attachments={[]} onRemove={() => {}} />);
    expect(container.firstChild).toBeNull();
  });

  it("falls back to the path's last segment when the name is empty", () => {
    render(
      <AttachmentChips
        attachments={[{ path: "/tmp/dir/fallback.pdf", name: "", size: 1024 * 1024 * 1.5 }]}
        onRemove={() => {}}
      />,
    );
    expect(screen.getByText("fallback.pdf")).toBeTruthy();
    expect(screen.getByRole("button", { name: "移除 fallback.pdf" })).toBeTruthy();
    expect(screen.getByText("1.5 MB")).toBeTruthy();
  });

  it("exposes the full path as a tooltip while truncating long names", () => {
    render(
      <AttachmentChips
        attachments={[{ path: "/tmp/a-very-long-dir-name/report.pdf", name: "report.pdf", size: 10 }]}
        onRemove={() => {}}
      />,
    );
    expect(screen.getByText("report.pdf").getAttribute("title")).toBe(
      "/tmp/a-very-long-dir-name/report.pdf",
    );
  });
});
