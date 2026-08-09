import { act, renderHook } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import {
  CANONICAL_UPLOAD_PATH,
  sanitizeUploadName,
  serializeComposer,
  tokenForFile,
  useSessionUploads,
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

  test("restores the original pending file when its path is pasted after removal", () => {
    const file = new File(["hello"], "notes.txt");
    const { result } = renderHook(() => useSessionUploads());
    let original: UploadToken;

    act(() => {
      [original] = result.current.addFiles([file]);
    });
    act(() => result.current.remove(original!.id));
    expect(result.current.tokens).toEqual([]);

    act(() => expect(result.current.addCanonicalPath(original!.path)).toBe(true));

    expect(result.current.tokens).toHaveLength(1);
    expect(result.current.tokens[0]).toMatchObject({
      id: original!.id,
      path: original!.path,
      file,
      status: "pending",
    });
  });

  test("does not claim that an unknown path is uploaded before a session exists", () => {
    const { result } = renderHook(() => useSessionUploads());
    const path = "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/unknown.txt";

    act(() => expect(result.current.addCanonicalPath(path)).toBe(false));

    expect(result.current.tokens).toEqual([]);
  });
});
