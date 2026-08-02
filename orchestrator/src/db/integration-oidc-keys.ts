/** KEK-sealed deployment OIDC signing keys with overlapping publication (ADR 0109). */

import { and, eq, gt, isNull, or } from "drizzle-orm";
import { createPublicKey, generateKeyPairSync } from "node:crypto";

import { getDb } from "./client.ts";
import { integrationOidcKey as keyTable } from "./schema.ts";
import { defaultSealer, type Sealer } from "../crypto/seal.ts";

export interface OidcSigningKey {
  kid: string;
  publicJwk: JsonWebKey;
  privateKeyPem: string;
  state: "active" | "retiring";
  createdAt: Date;
  publishUntil: Date | null;
}

export interface IntegrationOidcKeyStore {
  getOrCreateActive(now: Date): Promise<OidcSigningKey>;
  listPublished(now: Date): Promise<Array<Omit<OidcSigningKey, "privateKeyPem">>>;
  rotate(now: Date, overlapMs: number): Promise<OidcSigningKey>;
}

export interface IntegrationOidcKeyStoreDeps {
  sealer?: Sealer;
  randomId?: () => string;
  generateKeyPair?: () => { privateKeyPem: string; publicJwk: JsonWebKey };
}

function generateRsaKeyPair(): { privateKeyPem: string; publicJwk: JsonWebKey } {
  const pair = generateKeyPairSync("rsa", {
    modulusLength: 2048,
    publicKeyEncoding: { type: "spki", format: "pem" },
    privateKeyEncoding: { type: "pkcs8", format: "pem" },
  });
  return {
    privateKeyPem: pair.privateKey,
    publicJwk: createPublicKey(pair.publicKey).export({ format: "jwk" }),
  };
}
export function makeIntegrationOidcKeyStore(
  db: ReturnType<typeof getDb> = getDb(),
  deps: IntegrationOidcKeyStoreDeps = {},
): IntegrationOidcKeyStore {
  const sealer = deps.sealer ?? defaultSealer();
  const randomId = deps.randomId ?? (() => crypto.randomUUID());
  const generateKeyPair = deps.generateKeyPair ?? generateRsaKeyPair;

  function open(row: typeof keyTable.$inferSelect): OidcSigningKey {
    return {
      kid: row.kid,
      publicJwk: row.publicJwk as JsonWebKey,
      privateKeyPem: sealer.open({
        wrappedDek: row.wrappedDek,
        nonce: row.nonce,
        ciphertext: row.ciphertext,
        keyId: row.keyId,
      }).toString("utf8"),
      state: row.state,
      createdAt: row.createdAt,
      publishUntil: row.publishUntil ?? null,
    };
  }

  async function active(): Promise<OidcSigningKey | null> {
    const rows = await db.select().from(keyTable).where(eq(keyTable.state, "active")).limit(1);
    return rows[0] ? open(rows[0]) : null;
  }

  async function insertActive(now: Date): Promise<OidcSigningKey> {
    const kid = randomId();
    const pair = generateKeyPair();
    const sealed = sealer.seal(pair.privateKeyPem);
    const rows = await db.insert(keyTable).values({
      kid,
      publicJwk: { ...pair.publicJwk, kid, alg: "RS256", use: "sig" },
      wrappedDek: sealed.wrappedDek,
      nonce: sealed.nonce,
      ciphertext: sealed.ciphertext,
      keyId: sealed.keyId,
      state: "active",
      createdAt: now,
    }).returning();
    return open(rows[0]!);
  }

  return {
    async getOrCreateActive(now) {
      const existing = await active();
      if (existing) return existing;
      try {
        return await insertActive(now);
      } catch (error) {
        // Another pod can win the unique active-key insert.
        const winner = await active();
        if (winner) return winner;
        throw error;
      }
    },

    async listPublished(now) {
      const rows = await db.select().from(keyTable).where(
        or(
          eq(keyTable.state, "active"),
          and(eq(keyTable.state, "retiring"), or(isNull(keyTable.publishUntil), gt(keyTable.publishUntil, now))),
        ),
      );
      // JWKS publication needs public columns only. Do not unseal private keys
      // on this public read path.
      return rows.map((row) => ({
        kid: row.kid,
        publicJwk: row.publicJwk as JsonWebKey,
        state: row.state,
        createdAt: row.createdAt,
        publishUntil: row.publishUntil ?? null,
      }));
    },

    async rotate(now, overlapMs) {
      const publishUntil = new Date(now.getTime() + overlapMs);
      return db.transaction(async (tx) => {
        await tx.update(keyTable).set({ state: "retiring", publishUntil }).where(eq(keyTable.state, "active"));
        const kid = randomId();
        const pair = generateKeyPair();
        const sealed = sealer.seal(pair.privateKeyPem);
        const rows = await tx.insert(keyTable).values({
          kid,
          publicJwk: { ...pair.publicJwk, kid, alg: "RS256", use: "sig" },
          wrappedDek: sealed.wrappedDek,
          nonce: sealed.nonce,
          ciphertext: sealed.ciphertext,
          keyId: sealed.keyId,
          state: "active",
          createdAt: now,
        }).returning();
        return open(rows[0]!);
      });
    },
  };
}
