export function assertOwnerOrAdmin(
  ownerId: string | null,
  actorId: string,
  isAdmin: boolean,
): void {
  if (!actorId || (!isAdmin && (!ownerId || ownerId !== actorId)))
    throw new Error("session owner or administrator required");
}
