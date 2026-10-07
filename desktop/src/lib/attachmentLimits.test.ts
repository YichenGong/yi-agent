import { describe, expect, it } from "vitest";
import {
  ATTACHMENT_EXTENSIONS,
  attachmentProblem,
  fileNameOf,
  MAX_ATTACHMENT_BYTES,
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

describe("ATTACHMENT_EXTENSIONS", () => {
  it("covers the formats read_document supports", () => {
    for (const ext of ["pdf", "docx", "txt", "md", "csv", "html", "htm", "rtf"]) {
      expect(ATTACHMENT_EXTENSIONS).toContain(ext);
    }
  });
});
