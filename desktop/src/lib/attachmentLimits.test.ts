import { describe, expect, it } from "vitest";
import {
  ATTACHMENT_EXTENSIONS,
  attachmentProblem,
  fileNameOf,
  imageAttachmentProblem,
  MAX_ATTACHMENT_BYTES,
  pendingKindFromPath,
} from "./attachmentLimits";

describe("fileNameOf", () => {
  it("takes the last path segment on posix and windows", () => {
    expect(fileNameOf("/Users/me/报告.pdf")).toBe("报告.pdf");
    expect(fileNameOf("C:\\Users\\me\\report.docx")).toBe("report.docx");
  });
});

describe("attachmentProblem", () => {
  it("accepts a supported document under the size limit", () => {
    expect(attachmentProblem("/tmp/报告.pdf", 1024)).toBeNull();
  });

  it("rejects an unsupported extension", () => {
    const problem = attachmentProblem("/tmp/movie.mov", 1024);
    expect(problem).toContain("mov");
  });

  it("rejects a file over the limit and names the limit", () => {
    const problem = attachmentProblem("/tmp/报告.pdf", MAX_ATTACHMENT_BYTES + 1);
    expect(problem).not.toBeNull();
    expect(problem).toContain("50 MB");
  });

  it("is case-insensitive about the extension", () => {
    expect(attachmentProblem("/tmp/REPORT.PDF", 1)).toBeNull();
  });
});

describe("imageAttachmentProblem", () => {
  it("accepts images but rejects a document-only extension for the image picker", () => {
    expect(imageAttachmentProblem("/tmp/a.png", 100)).toBeNull();
    expect(imageAttachmentProblem("/tmp/a.jpg", 100)).toBeNull();
    expect(imageAttachmentProblem("/tmp/a.pdf", 100)).toMatch(/不支持/);
    expect(imageAttachmentProblem("/tmp/a.png", 21 * 1024 * 1024)).toMatch(/20 MB/);
  });
  it("is case-insensitive and names the limit", () => {
    expect(imageAttachmentProblem("/tmp/A.PNG", 1)).toBeNull();
    expect(imageAttachmentProblem("/tmp/a.txt", 1)).toMatch(/不支持/);
    expect(imageAttachmentProblem("/tmp/a.png", 20 * 1024 * 1024)).toBeNull();
  });
});

describe("pendingKindFromPath", () => {
  it("classifies by extension, case-insensitively, over disjoint lists", () => {
    expect(pendingKindFromPath("/tmp/a.PNG")).toBe("image");
    expect(pendingKindFromPath("/tmp/a.jpeg")).toBe("image");
    expect(pendingKindFromPath("/tmp/报告.pdf")).toBe("document");
    expect(pendingKindFromPath("/tmp/notes.md")).toBe("document");
  });
});

describe("ATTACHMENT_EXTENSIONS", () => {
  it("covers the formats read_document supports", () => {
    for (const ext of ["pdf", "docx", "txt", "md", "csv", "html", "htm", "rtf"]) {
      expect(ATTACHMENT_EXTENSIONS).toContain(ext);
    }
  });
});
