import { expect, test, describe } from "bun:test";
import { ConnectError, Code } from "@connectrpc/connect";
import { makeDisableImageGuard } from "../rpc/image-guard.ts";
import type { ProfileStore, ProfileRow } from "../db/profiles.ts";
import type { ImagesClient } from "../rpc/tasks.ts";

const images: ImagesClient = {
  async listEnabledImages() {
    return { images: [{ id: "img-1", imageUri: "registry/api:latest" }] };
  },
};

function storeWith(profiles: Partial<ProfileRow>[]): ProfileStore {
  const rows = profiles.map((p, i) => ({
    id: `p${i}`, name: p.name ?? `P${i}`, description: "", icon: "Bot",
    imageId: p.imageId ?? "img-1", includeUserTokens: false, envVars: {},
    createdAt: new Date(0), updatedAt: new Date(0), deletedAt: p.deletedAt ?? null,
  })) as ProfileRow[];
  return {
    async list({ includeArchived }) { return rows.filter((r) => includeArchived || !r.deletedAt); },
    async get() { return null; }, async getActive() { return null; }, async getDefault() { return null; }, async getByIds() { return []; },
    async create() { throw new Error("unused"); }, async update() { return null; }, async softDelete() {},
  };
}

const ctx = {} as never;

describe("DisableImage guard", () => {
  test("blocks when an active profile references the image", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Backend", imageId: "img-1" }]) });
    try {
      await guard({ imageUri: "registry/api:latest" }, ctx);
      throw new Error("expected to throw");
    } catch (e) {
      expect(e).toBeInstanceOf(ConnectError);
      expect((e as ConnectError).code).toBe(Code.FailedPrecondition);
      expect((e as ConnectError).message).toContain("Backend");
    }
  });

  test("allows when only an archived profile references it", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Old", imageId: "img-1", deletedAt: new Date(0) }]) });
    await guard({ imageUri: "registry/api:latest" }, ctx); // no throw
  });

  test("allows when no profile references it", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Other", imageId: "img-2" }]) });
    await guard({ imageUri: "registry/api:latest" }, ctx); // no throw
  });

  test("no-op for an unknown / already-disabled uri", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Backend", imageId: "img-1" }]) });
    await guard({ imageUri: "registry/gone:latest" }, ctx); // not in catalog → no throw
  });
});
