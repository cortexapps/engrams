const COLLABORATOR_PALETTE = [
  "#2563eb",
  "#7c3aed",
  "#b85c0a",
  "#3a6b5c",
  "#be123c",
  "#4f46e5",
] as const;

/** A stable neutral for a historical author whose account no longer exists. */
export const MISSING_COLLABORATOR_COLOR = "#64748b";

/** Assign a stable identity color that is safe for Yjs awareness payloads. */
export function collaboratorColor(userId: string | null): string {
  if (userId === null) return MISSING_COLLABORATOR_COLOR;
  let hash = 0;
  for (const character of userId) hash = (hash * 31 + character.charCodeAt(0)) >>> 0;
  return COLLABORATOR_PALETTE[hash % COLLABORATOR_PALETTE.length];
}
