import { expect, test, describe } from "bun:test";
import { tarSingleFile } from "../routes/skill-tar.ts";

function readCString(buf: Uint8Array, off: number, len: number): string {
  let end = off;
  while (end < off + len && buf[end] !== 0) end++;
  return new TextDecoder().decode(buf.subarray(off, end));
}

describe("tarSingleFile", () => {
  test("produces a parseable single-entry ustar that round-trips", () => {
    const data = new TextEncoder().encode("# My Skill\nDo the thing.\n");
    const tar = tarSingleFile("SKILL.md", data);

    // 512 header + 1 data block (data < 512) + 1024 zero trailer.
    expect(tar.length).toBe(512 + 512 + 1024);
    expect(readCString(tar, 0, 100)).toBe("SKILL.md");
    expect(readCString(tar, 257, 6)).toBe("ustar");
    expect(tar[156]).toBe(0x30); // typeflag '0' = regular file

    const size = parseInt(readCString(tar, 124, 12).trim(), 8);
    expect(size).toBe(data.length);
    expect(new TextDecoder().decode(tar.subarray(512, 512 + data.length))).toBe(
      "# My Skill\nDo the thing.\n",
    );

    // The stored checksum equals a recomputation (chksum field as spaces).
    const stored = parseInt(readCString(tar, 148, 8).trim(), 8);
    const header = tar.slice(0, 512);
    for (let i = 148; i < 156; i++) header[i] = 0x20;
    let sum = 0;
    for (let i = 0; i < 512; i++) sum += header[i];
    expect(sum).toBe(stored);

    // The trailer is all zeros.
    expect(tar.subarray(1024).every((b) => b === 0)).toBe(true);
  });

  test("rejects an over-long entry name", () => {
    expect(() => tarSingleFile("x".repeat(101), new Uint8Array(0))).toThrow();
  });
});
