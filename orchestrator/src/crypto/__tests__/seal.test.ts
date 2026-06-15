/**
 * Tests for the KEK-envelope seal/open crypto (ADR 0051).
 *
 * SECURITY-CRITICAL: these guard the byte-for-byte compatibility with the Rust
 * coordinator's `engram-crypto`. A fixed test key is injected via makeSealer so
 * the tests never depend on ENGRAM_KEK_MASTER_KEY being set.
 */

import { describe, test, expect } from "bun:test";

import {
  makeSealer,
  KEK_KEY_ID,
  type SealedSecret,
} from "../seal.ts";

// A fixed, valid 32-byte base64 KEK for tests (0x00..0x1f).
const TEST_KEK = Buffer.from(
  Array.from({ length: 32 }, (_, i) => i),
).toString("base64");

// A second, different 32-byte key (all 0xaa) for the wrong-KEK test.
const OTHER_KEK = Buffer.alloc(32, 0xaa).toString("base64");

describe("seal / open round-trip", () => {
  const cases: Array<[string, string]> = [
    ["ascii", "sk-ant-oat01-hunter2"],
    ["unicode", "héllo 🌍 — токен — 秘密"],
    ["empty", ""],
    ["long", "x".repeat(8192)],
  ];

  for (const [name, plaintext] of cases) {
    test(`round-trips: ${name}`, () => {
      const sealer = makeSealer(TEST_KEK);
      const sealed = sealer.seal(plaintext);

      // keyId is the env-keyed identifier matching Rust EnvVarKeyProvider.
      expect(sealed.keyId).toBe(KEK_KEY_ID);
      // ciphertext must not contain the plaintext bytes.
      if (plaintext.length > 0) {
        expect(sealed.ciphertext.includes(Buffer.from(plaintext, "utf8"))).toBe(
          false,
        );
      }
      // wrappedDek layout: nonce(12) + ct + tag(16); a 32-byte DEK → 60 bytes.
      expect(sealed.wrappedDek.length).toBe(12 + 32 + 16);
      // value nonce is 12 bytes.
      expect(sealed.nonce.length).toBe(12);
      // ciphertext = ct(len) + tag(16); for empty plaintext that's 16 bytes.
      expect(sealed.ciphertext.length).toBe(
        Buffer.from(plaintext, "utf8").length + 16,
      );

      const opened = sealer.open(sealed);
      expect(opened.toString("utf8")).toBe(plaintext);
    });
  }
});

describe("tamper / wrong-key rejection (GCM auth)", () => {
  test("flipping a ciphertext byte → open throws", () => {
    const sealer = makeSealer(TEST_KEK);
    const sealed = sealer.seal("important");
    // Flip the first ciphertext byte.
    const tampered: SealedSecret = {
      ...sealed,
      ciphertext: Buffer.from(sealed.ciphertext),
    };
    tampered.ciphertext[0] ^= 0x01;
    expect(() => sealer.open(tampered)).toThrow();
  });

  test("flipping a wrappedDek byte → open throws", () => {
    const sealer = makeSealer(TEST_KEK);
    const sealed = sealer.seal("important");
    const tampered: SealedSecret = {
      ...sealed,
      wrappedDek: Buffer.from(sealed.wrappedDek),
    };
    // Flip a byte inside the wrapped DEK ciphertext (past the 12-byte nonce).
    tampered.wrappedDek[20] ^= 0x01;
    expect(() => sealer.open(tampered)).toThrow();
  });

  test("wrong KEK → open throws", () => {
    const sealerA = makeSealer(TEST_KEK);
    const sealerB = makeSealer(OTHER_KEK);
    const sealed = sealerA.seal("secret");
    // Different KEK → DEK unwrap fails GCM auth → throws.
    expect(() => sealerB.open(sealed)).toThrow();
  });
});

describe("non-determinism (fresh DEK + nonce per seal)", () => {
  test("two seals of the same plaintext differ in dek/nonce/ciphertext", () => {
    const sealer = makeSealer(TEST_KEK);
    const a = sealer.seal("same plaintext");
    const b = sealer.seal("same plaintext");
    expect(a.wrappedDek.equals(b.wrappedDek)).toBe(false);
    expect(a.nonce.equals(b.nonce)).toBe(false);
    expect(a.ciphertext.equals(b.ciphertext)).toBe(false);
    // ...but both decrypt to the same plaintext.
    expect(sealer.open(a).toString("utf8")).toBe("same plaintext");
    expect(sealer.open(b).toString("utf8")).toBe("same plaintext");
  });
});

describe("KEK validation", () => {
  test("non-32-byte KEK throws at construction", () => {
    const short = Buffer.alloc(16, 0).toString("base64");
    expect(() => makeSealer(short)).toThrow();
  });

  test("accepts a Buffer plaintext and round-trips identical bytes", () => {
    const sealer = makeSealer(TEST_KEK);
    const raw = Buffer.from([0x00, 0xff, 0x10, 0x7f, 0x80]);
    const sealed = sealer.seal(raw);
    expect(sealer.open(sealed).equals(raw)).toBe(true);
  });
});
