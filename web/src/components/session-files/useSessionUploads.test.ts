import { describe, expect, test } from "vitest";

import {
  CANONICAL_UPLOAD_PATH,
  sanitizeUploadName,
  serializeComposer,
  tokenForFile,
  type UploadToken,
} from "./useSessionUploads";

describe("session upload composer tokens", () => {
  test("mints a canonical path and sanitizes the browser file name", () => {
    const token = tokenForFile(new File(["hello"], "design notes/日本語.pdf"));
    expect(token.name).toBe("design_notes____.pdf");
    expect(token.status).toBe("pending");
    expect(CANONICAL_UPLOAD_PATH.test(token.path)).toBe(true);
    expect(token.path.endsWith(`/${token.name}`)).toBe(true);
  });

  test("rejects traversal spellings through sanitization", () => {
    expect(sanitizeUploadName("../secret")).toBe(".._secret");
    expect(sanitizeUploadName(".")).toBe("upload");
    expect(sanitizeUploadName("..")).toBe("upload");
  });

  test("serializes chip paths as literal prompt text in token order", () => {
    const tokens: UploadToken[] = [
      {
        id: "019fe2ff-0464-75f3-bb20-a8c1844579b9",
        name: "a.txt",
        path: "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/a.txt",
        status: "uploaded",
        progress: 1,
      },
      {
        id: "119fe2ff-0464-75f3-bb20-a8c1844579b9",
        name: "b.txt",
        path: "/tmp/uploads/119fe2ff-0464-75f3-bb20-a8c1844579b9/b.txt",
        status: "uploaded",
        progress: 1,
      },
    ];
    expect(serializeComposer("Review these files", tokens)).toBe(
      `Review these files\n${tokens[0]!.path}\n${tokens[1]!.path}`,
    );
    expect(serializeComposer(`Review ${tokens[0]!.path} first`, tokens)).toBe(
      `Review ${tokens[0]!.path} first\n${tokens[1]!.path}`,
    );
  });
});
