/**
 * DisableImage profile guard (ADR 0052 §3).
 *
 * The coordinator 409s DisableImage when live SESSIONS reference an image, but
 * it knows nothing about orchestrator PROFILES. This pre-flight (run on the
 * passthrough before forwarding) blocks disabling an image that any ACTIVE
 * profile references, with failed_precondition + the blocking profile names.
 *
 * DisableImageRequest carries image_uri, while profiles store image_id — so we
 * resolve image_uri -> enabled_images.id via ListEnabledImages first. If the
 * uri isn't in the catalog (already disabled / unknown), we don't block and let
 * the upstream handle it (idempotent 204).
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { HandlerContext } from "@connectrpc/connect";

import { getDb } from "../db/client.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { images as defaultImages } from "../control-plane/client.ts";
import type { ImagesClient } from "./tasks.ts";

export function makeDisableImageGuard(deps?: {
  store?: ProfileStore;
  images?: ImagesClient;
}): (req: unknown, ctx: HandlerContext) => Promise<void> {
  const store: ProfileStore = deps?.store ?? makeProfileStore(getDb());
  const images: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);

  return async (req: unknown) => {
    const imageUri = (req as { imageUri?: string }).imageUri;
    if (!imageUri) return;

    const catalog = await images.listEnabledImages({});
    const match = catalog.images.find((i) => i.imageUri === imageUri);
    if (!match) return; // not enabled / unknown → let upstream be idempotent

    const active = (await store.list({ includeArchived: false })).filter((p) => p.imageId === match.id);
    if (active.length > 0) {
      const names = active.map((p) => p.name).join(", ");
      throw new ConnectError(
        `Can't disable — ${active.length} profile${active.length === 1 ? "" : "s"} use this image: ${names}`,
        Code.FailedPrecondition,
      );
    }
  };
}
