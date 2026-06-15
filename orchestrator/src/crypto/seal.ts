/**
 * KEK-envelope encryption for orchestrator-managed secrets at rest (ADR 0051).
 *
 * SECURITY-CRITICAL: this is a byte-for-byte reimplementation of the Rust
 * coordinator's `engram-crypto` `CredCipher` + `EnvVarKeyProvider`. A value
 * sealed here (with a given KEK) is format-identical to one sealed by the
 * coordinator with the SAME key, and vice-versa. Both share the engrams
 * encryption key (`ENGRAM_KEK_MASTER_KEY`, base64 → 32 bytes; in prod sourced
 * from GCP Secret Manager).
 *
 * Envelope shape (mirrors engram-crypto):
 *   - Per-secret AES-256-GCM data key (DEK, 32 random bytes), wrapped by the
 *     KEK. The KEK never touches plaintext secrets — it only wraps DEKs.
 *   - `SealedSecret = { wrappedDek, nonce, ciphertext, keyId }`, stored as four
 *     individually-queryable columns (matching the Rust `SealedCred`).
 *
 * Byte layout (MUST match Rust exactly):
 *   - ciphertext  = AES-256-GCM(DEK, nonce, plaintext) ‖ tag(16)
 *       RustCrypto's `aes-gcm` appends the GCM tag to the ciphertext; node's
 *       `getAuthTag()` returns it separately, so we Buffer.concat it on.
 *   - wrappedDek  = dekNonce(12) ‖ AES-256-GCM(KEK, dekNonce, DEK) ‖ wtag(16)
 *       i.e. EnvVarKeyProvider::wrap's `[nonce | AES-GCM(KEK, nonce, DEK)]`,
 *       where the trailing 16 bytes of the GCM output are the tag.
 *   - nonce       = the 12-byte value nonce used for the ciphertext.
 *   - keyId       = "env:ENGRAM_KEK_MASTER_KEY:v1" (matches `EnvVarKeyProvider::from_env`).
 *
 * The KEK is injectable via `makeSealer(kekBase64)` (tests pass a fixed key).
 * The default sealer reads `config.kekMasterKey` (from ENGRAM_KEK_MASTER_KEY).
 */

import {
  createCipheriv,
  createDecipheriv,
  randomBytes,
} from "node:crypto";

import { config } from "../config.ts";

/** The env var holding the base64-encoded 32-byte KEK (shared with the coordinator). */
export const KEK_ENV_VAR = "ENGRAM_KEK_MASTER_KEY";

/**
 * Stable identifier for the active KEK, stored alongside each sealed secret.
 * MUST match Rust `EnvVarKeyProvider::from_env`'s `format!("env:{var_name}:v1")`.
 */
export const KEK_KEY_ID = `env:${KEK_ENV_VAR}:v1`;

/** GCM tag length in bytes (AES-256-GCM, RustCrypto default). */
const TAG_LEN = 16;
/** GCM nonce length in bytes (12 = 96 bits, AES-256-GCM standard). */
const NONCE_LEN = 12;
/** DEK length in bytes (AES-256 key). */
const DEK_LEN = 32;

/**
 * A sealed secret: AES-256-GCM ciphertext (with appended tag) + the wrapped DEK
 * that produced it + the value nonce + the KEK identifier in effect at seal
 * time. Mirrors Rust `SealedCred`.
 */
export interface SealedSecret {
  /** dekNonce(12) ‖ AES-GCM(KEK, dekNonce, DEK) ‖ wtag(16). */
  wrappedDek: Buffer;
  /** The 12-byte value nonce used to encrypt the plaintext. */
  nonce: Buffer;
  /** AES-GCM(DEK, nonce, plaintext) ‖ tag(16). */
  ciphertext: Buffer;
  /** KEK identifier (e.g. "env:ENGRAM_KEK_MASTER_KEY:v1"). */
  keyId: string;
}

/** The seal/open seam. Construct via `makeSealer` or use the default `sealer`. */
export interface Sealer {
  /** Generate a fresh DEK+nonce, encrypt + wrap, tag with the active keyId. */
  seal(plaintext: string | Buffer): SealedSecret;
  /** Unwrap the DEK and decrypt. Throws on wrong KEK / tampered bytes (GCM auth). */
  open(sealed: SealedSecret): Buffer;
  /** The active KEK identifier. */
  readonly keyId: string;
}

/**
 * Decode the base64 KEK and assert it is exactly 32 bytes. Padding-tolerant
 * (Buffer.from(..., 'base64') accepts both padded and unpadded), matching the
 * Rust `from_base64` STANDARD/STANDARD_NO_PAD fallback.
 */
function decodeKek(kekBase64: string): Buffer {
  const kek = Buffer.from(kekBase64.trim(), "base64");
  if (kek.length !== DEK_LEN) {
    throw new Error(
      `${KEK_ENV_VAR} must decode to exactly 32 bytes (got ${kek.length}); ` +
        `generate one with: head -c 32 /dev/urandom | base64`,
    );
  }
  return kek;
}

/**
 * Build a `Sealer` bound to the given base64 KEK. Throws immediately if the KEK
 * is not 32 bytes after base64-decode (fail closed at construction).
 *
 * Injectable so tests pass a fixed key; the default `sealer` reads
 * `config.kekMasterKey`.
 */
export function makeSealer(kekBase64: string): Sealer {
  const kek = decodeKek(kekBase64);

  return {
    keyId: KEK_KEY_ID,

    seal(plaintext: string | Buffer): SealedSecret {
      const pt = Buffer.isBuffer(plaintext)
        ? plaintext
        : Buffer.from(plaintext, "utf8");

      // 1. Fresh DEK + value nonce.
      const dek = randomBytes(DEK_LEN);
      const valueNonce = randomBytes(NONCE_LEN);

      // 2. Encrypt plaintext with the DEK. RustCrypto appends the GCM tag to
      //    the ciphertext, so we Buffer.concat([ct, tag]) to match.
      const cipher = createCipheriv("aes-256-gcm", dek, valueNonce);
      const ct = Buffer.concat([cipher.update(pt), cipher.final()]);
      const tag = cipher.getAuthTag(); // 16 bytes
      const ciphertext = Buffer.concat([ct, tag]);

      // 3. Wrap the DEK with the KEK. Layout: dekNonce(12) ‖ wct ‖ wtag(16),
      //    matching EnvVarKeyProvider::wrap's [nonce | AES-GCM(KEK,nonce,DEK)].
      const dekNonce = randomBytes(NONCE_LEN);
      const wrapCipher = createCipheriv("aes-256-gcm", kek, dekNonce);
      const wct = Buffer.concat([wrapCipher.update(dek), wrapCipher.final()]);
      const wtag = wrapCipher.getAuthTag(); // 16 bytes
      const wrappedDek = Buffer.concat([dekNonce, wct, wtag]);

      return { wrappedDek, nonce: valueNonce, ciphertext, keyId: KEK_KEY_ID };
    },

    open(sealed: SealedSecret): Buffer {
      // 1. Parse wrappedDek: dekNonce(12) ‖ wct ‖ wtag(16).
      if (sealed.wrappedDek.length < NONCE_LEN + TAG_LEN) {
        throw new Error(
          `wrapped DEK too short (${sealed.wrappedDek.length} bytes)`,
        );
      }
      const dekNonce = sealed.wrappedDek.subarray(0, NONCE_LEN);
      const wtag = sealed.wrappedDek.subarray(sealed.wrappedDek.length - TAG_LEN);
      const wct = sealed.wrappedDek.subarray(
        NONCE_LEN,
        sealed.wrappedDek.length - TAG_LEN,
      );

      // Unwrap the DEK. A wrong KEK or tampered wrap throws (GCM auth failure).
      const unwrap = createDecipheriv("aes-256-gcm", kek, dekNonce);
      unwrap.setAuthTag(wtag);
      const dek = Buffer.concat([unwrap.update(wct), unwrap.final()]);
      if (dek.length !== DEK_LEN) {
        throw new Error(
          `unwrapped DEK has wrong length (${dek.length}, expected ${DEK_LEN})`,
        );
      }

      // 2. Parse ciphertext: ct ‖ tag(16).
      if (sealed.ciphertext.length < TAG_LEN) {
        throw new Error(
          `ciphertext too short (${sealed.ciphertext.length} bytes)`,
        );
      }
      const ct = sealed.ciphertext.subarray(
        0,
        sealed.ciphertext.length - TAG_LEN,
      );
      const tag = sealed.ciphertext.subarray(
        sealed.ciphertext.length - TAG_LEN,
      );

      // Decrypt. A wrong DEK (wrong KEK) or tampered ciphertext/nonce throws
      // on final() (GCM auth failure) — we let it throw, never return garbage.
      const decipher = createDecipheriv("aes-256-gcm", dek, sealed.nonce);
      decipher.setAuthTag(tag);
      return Buffer.concat([decipher.update(ct), decipher.final()]);
    },
  };
}

// ---------------------------------------------------------------------------
// Default sealer — reads the KEK from config (ENGRAM_KEK_MASTER_KEY).
//
// Constructed lazily so importing this module does not require the env var at
// import time (tests inject a fixed key via makeSealer). The first call to
// `sealer()` resolves config.kekMasterKey and validates it (throws if not 32
// bytes), failing closed.
// ---------------------------------------------------------------------------

let cachedDefault: Sealer | undefined;

/** The process-default `Sealer`, lazily built from `config.kekMasterKey`. */
export function defaultSealer(): Sealer {
  if (!cachedDefault) {
    cachedDefault = makeSealer(config.kekMasterKey);
  }
  return cachedDefault;
}
