/**
 * Minimal single-file POSIX ustar tar builder (ADR 0055 P2).
 *
 * The coordinator's skill packer takes a tar of the skill directory and lays it
 * under `skills/<name>/`. A power user uploads a `.tar.gz` (forwarded verbatim —
 * the coordinator gunzips), but the common case is a lone `SKILL.md`; this wraps
 * that single file into a one-entry uncompressed ustar so the coordinator sees a
 * uniform tar either way. Deterministic (zeroed mtime/ids) — no external dep.
 */

const BLOCK = 512;

function writeAscii(buf: Uint8Array, offset: number, max: number, s: string): void {
  const bytes = new TextEncoder().encode(s);
  buf.set(bytes.subarray(0, max), offset);
}

/** Left-pad an octal number to `digits` then NUL-terminate (tar numeric field). */
function octalField(n: number, digits: number): string {
  return n.toString(8).padStart(digits, "0") + "\0";
}

/**
 * Build a one-entry ustar archive containing `name` with `data`.
 * `name` must be a safe relative path ≤ 100 bytes (e.g. "SKILL.md").
 */
export function tarSingleFile(name: string, data: Uint8Array): Uint8Array {
  if (new TextEncoder().encode(name).length > 100) {
    throw new Error("tar entry name exceeds 100 bytes");
  }
  const header = new Uint8Array(BLOCK);

  writeAscii(header, 0, 100, name); // name
  writeAscii(header, 100, 8, octalField(0o644, 7)); // mode
  writeAscii(header, 108, 8, octalField(0, 7)); // uid
  writeAscii(header, 116, 8, octalField(0, 7)); // gid
  writeAscii(header, 124, 12, octalField(data.length, 11)); // size
  writeAscii(header, 136, 12, octalField(0, 11)); // mtime (deterministic)
  // chksum (148..156): spaces while computing.
  for (let i = 148; i < 156; i++) header[i] = 0x20;
  header[156] = 0x30; // typeflag '0' = regular file
  writeAscii(header, 257, 6, "ustar\0"); // magic
  writeAscii(header, 263, 2, "00"); // version

  // Checksum: unsigned sum of all header bytes (chksum field as spaces),
  // written as 6 octal digits + NUL + space.
  let sum = 0;
  for (let i = 0; i < BLOCK; i++) sum += header[i];
  writeAscii(header, 148, 8, sum.toString(8).padStart(6, "0") + "\0 ");

  // Data, padded to a block boundary, then two zero blocks (end of archive).
  const dataBlocks = Math.ceil(data.length / BLOCK);
  const out = new Uint8Array(BLOCK + dataBlocks * BLOCK + 2 * BLOCK);
  out.set(header, 0);
  out.set(data, BLOCK);
  return out;
}
