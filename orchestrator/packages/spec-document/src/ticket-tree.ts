/**
 * The ticket tree, as pure data (ADR 0114 D6, R39-R40).
 *
 * After publish the canvas becomes a tree of proposed tickets, and hand
 * editing leads: retitle, edit, split, merge, reorder, re-parent, add, delete.
 * The agent can reshape the same tree from chat.
 *
 * Every operation here is a pure function from one node list to the next. The
 * browser runs them to move a row the instant a person drops it, and the
 * orchestrator runs the same functions inside the transaction that writes the
 * result. One implementation means an optimistic drag and the stored tree can
 * never disagree about what a move means.
 *
 * Two invariants hold after every operation:
 *
 *  1. Sibling ordinals are dense and start at 0.
 *  2. The parent links form a forest — no cycle, no orphan.
 *
 * `normalizeTree` restores both, and every operation ends with it.
 */

/** How far one ticket has travelled towards the issue tracker (R41). */
export type SpecTicketSyncState = "draft" | "queued" | "syncing" | "synced" | "failed";

/** One node of the tree. The field names match the `spec_ticket_draft` row. */
export interface SpecTicketNode {
  id: string;
  parentId: string | null;
  ordinal: number;
  title: string;
  /** Markdown, opening with the link back to the pinned section. */
  description: string;
  /** The §backlink: a section of the pinned spec. */
  sectionId: string;
  dependsOn: string[];
  syncState: SpecTicketSyncState;
  linearId: string | null;
  syncError: string | null;
}

export type SpecTicketTreeErrorCode =
  | "not_found"
  | "unknown_section"
  | "cycle"
  | "empty_merge"
  | "empty_split";

/** A tree operation that the tree itself refuses. */
export class SpecTicketTreeError extends Error {
  constructor(
    readonly code: SpecTicketTreeErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "SpecTicketTreeError";
  }
}

function requireNode(nodes: readonly SpecTicketNode[], id: string): SpecTicketNode {
  const node = nodes.find((candidate) => candidate.id === id);
  if (!node) throw new SpecTicketTreeError("not_found", `Unknown ticket: ${id}`);
  return node;
}

/** The children of one parent, in ordinal order. */
export function childrenOf(
  nodes: readonly SpecTicketNode[],
  parentId: string | null,
): SpecTicketNode[] {
  return nodes
    .filter((node) => node.parentId === parentId)
    .sort((left, right) => left.ordinal - right.ordinal || left.id.localeCompare(right.id));
}

/**
 * Depth-first order: a parent, then its subtree, then its next sibling. This
 * is the order the canvas renders and the order a person reads.
 */
export function orderedTree(nodes: readonly SpecTicketNode[]): SpecTicketNode[] {
  const ordered: SpecTicketNode[] = [];
  const seen = new Set<string>();
  const walk = (parentId: string | null): void => {
    for (const node of childrenOf(nodes, parentId)) {
      if (seen.has(node.id)) continue; // a cycle cannot recur forever
      seen.add(node.id);
      ordered.push(node);
      walk(node.id);
    }
  };
  walk(null);
  // A node whose parent is gone is still the person's work. Keep it, at the
  // root, rather than dropping it out of the tree.
  for (const node of nodes) {
    if (!seen.has(node.id)) {
      seen.add(node.id);
      ordered.push({ ...node, parentId: null });
    }
  }
  return ordered;
}

/** The depth of every node, root nodes at 0. */
export function treeDepths(nodes: readonly SpecTicketNode[]): Map<string, number> {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const depths = new Map<string, number>();
  for (const node of orderedTree(nodes)) {
    const parent = node.parentId === null ? null : byId.get(node.parentId);
    depths.set(node.id, parent ? (depths.get(parent.id) ?? 0) + 1 : 0);
  }
  return depths;
}

/** Every descendant of `id`, excluding `id` itself. */
export function descendantIds(nodes: readonly SpecTicketNode[], id: string): Set<string> {
  const found = new Set<string>();
  const walk = (parentId: string): void => {
    for (const child of childrenOf(nodes, parentId)) {
      if (found.has(child.id)) continue;
      found.add(child.id);
      walk(child.id);
    }
  };
  walk(id);
  return found;
}

/**
 * Rewrite sibling ordinals dense from 0, keeping depth-first order, and drop
 * every reference to a ticket that is no longer in the tree.
 */
export function normalizeTree(nodes: readonly SpecTicketNode[]): SpecTicketNode[] {
  const ordered = orderedTree(nodes);
  const live = new Set(ordered.map((node) => node.id));
  const nextOrdinal = new Map<string | null, number>();
  return ordered.map((node) => {
    const ordinal = nextOrdinal.get(node.parentId) ?? 0;
    nextOrdinal.set(node.parentId, ordinal + 1);
    const dependsOn = node.dependsOn.filter((id) => id !== node.id && live.has(id));
    return { ...node, ordinal, dependsOn };
  });
}

export interface AddTicketInput {
  id: string;
  parentId: string | null;
  /** Where among the new siblings. Past the end, or absent, means last. */
  index?: number;
  title: string;
  description: string;
  sectionId: string;
  dependsOn?: string[];
}

/** Add one ticket. It starts as a draft, with no sync state of its own. */
export function addTicket(
  nodes: readonly SpecTicketNode[],
  input: AddTicketInput,
): SpecTicketNode[] {
  if (input.parentId !== null) requireNode(nodes, input.parentId);
  const added: SpecTicketNode = {
    id: input.id,
    parentId: input.parentId,
    ordinal: 0,
    title: input.title,
    description: input.description,
    sectionId: input.sectionId,
    dependsOn: input.dependsOn ?? [],
    syncState: "draft",
    linearId: null,
    syncError: null,
  };
  return normalizeTree(spliceSibling(nodes, added, input.parentId, input.index));
}

export interface UpdateTicketInput {
  id: string;
  title?: string;
  description?: string;
  sectionId?: string;
  dependsOn?: string[];
}

/** Retitle, rewrite the description, or re-point the backlink. */
export function updateTicket(
  nodes: readonly SpecTicketNode[],
  input: UpdateTicketInput,
): SpecTicketNode[] {
  requireNode(nodes, input.id);
  return normalizeTree(
    nodes.map((node) =>
      node.id === input.id
        ? {
            ...node,
            ...(input.title === undefined ? {} : { title: input.title }),
            ...(input.description === undefined ? {} : { description: input.description }),
            ...(input.sectionId === undefined ? {} : { sectionId: input.sectionId }),
            ...(input.dependsOn === undefined ? {} : { dependsOn: input.dependsOn }),
          }
        : node,
    ),
  );
}

/** Delete one ticket and everything nested under it. */
export function deleteTicket(nodes: readonly SpecTicketNode[], id: string): SpecTicketNode[] {
  requireNode(nodes, id);
  const gone = descendantIds(nodes, id);
  gone.add(id);
  return normalizeTree(nodes.filter((node) => !gone.has(node.id)));
}

export interface MoveTicketInput {
  id: string;
  /** The new parent. `null` moves the ticket to the root. */
  parentId: string | null;
  /** The position among the new siblings, excluding the moved ticket. */
  index?: number;
}

/**
 * Reorder and re-parent in one verb, because a drag is one gesture: the
 * person drops a row at a depth and a position at the same moment.
 */
export function moveTicket(
  nodes: readonly SpecTicketNode[],
  input: MoveTicketInput,
): SpecTicketNode[] {
  const moved = requireNode(nodes, input.id);
  if (input.parentId !== null) {
    requireNode(nodes, input.parentId);
    if (input.parentId === input.id || descendantIds(nodes, input.id).has(input.parentId)) {
      throw new SpecTicketTreeError("cycle", "A ticket cannot be nested under its own subtree.");
    }
  }
  const rest = nodes.filter((node) => node.id !== input.id);
  return normalizeTree(
    spliceSibling(rest, { ...moved, parentId: input.parentId }, input.parentId, input.index),
  );
}

export interface SplitPart {
  id: string;
  title: string;
  description: string;
  sectionId?: string;
}

/**
 * Split one ticket into several siblings at its place. The children of the
 * original stay with the first part, so a split never orphans a subtree.
 */
export function splitTicket(
  nodes: readonly SpecTicketNode[],
  id: string,
  parts: readonly SplitPart[],
): SpecTicketNode[] {
  const source = requireNode(nodes, id);
  if (parts.length < 2) {
    throw new SpecTicketTreeError("empty_split", "A split needs at least two parts.");
  }
  const [first, ...others] = parts as [SplitPart, ...SplitPart[]];
  const replacement: SpecTicketNode[] = [
    {
      ...source,
      title: first.title,
      description: first.description,
      sectionId: first.sectionId ?? source.sectionId,
      // The split parts are new work, so they carry no sync identity.
      syncState: "draft",
      linearId: null,
      syncError: null,
    },
    ...others.map((part, offset) => ({
      id: part.id,
      parentId: source.parentId,
      ordinal: source.ordinal + offset + 1,
      title: part.title,
      description: part.description,
      sectionId: part.sectionId ?? source.sectionId,
      dependsOn: [] as string[],
      syncState: "draft" as SpecTicketSyncState,
      linearId: null,
      syncError: null,
    })),
  ];
  const shifted = nodes.map((node) =>
    node.id === source.id
      ? node
      : node.parentId === source.parentId && node.ordinal > source.ordinal
        ? { ...node, ordinal: node.ordinal + others.length }
        : node,
  );
  return normalizeTree([
    ...shifted.filter((node) => node.id !== source.id),
    ...replacement,
  ]);
}

export interface MergeTicketsInput {
  /** The survivor. It keeps its place, its backlink and its sync identity. */
  targetId: string;
  /** The tickets folded into the target, in the order they are folded. */
  sourceIds: readonly string[];
  /** The merged title. The target's title stays when this is absent. */
  title?: string;
  /** The merged description. The parts are joined when this is absent. */
  description?: string;
}

/**
 * Merge several tickets into one. The target keeps its place in the tree and
 * its backlink; each source's children move under the target, so a merge
 * never loses a subtree either.
 */
export function mergeTickets(
  nodes: readonly SpecTicketNode[],
  input: MergeTicketsInput,
): SpecTicketNode[] {
  const target = requireNode(nodes, input.targetId);
  const sources = input.sourceIds.filter((id) => id !== input.targetId);
  if (sources.length === 0) {
    throw new SpecTicketTreeError("empty_merge", "A merge needs a ticket to fold in.");
  }
  const merged = sources.map((id) => requireNode(nodes, id));
  for (const source of merged) {
    if (descendantIds(nodes, source.id).has(target.id)) {
      throw new SpecTicketTreeError(
        "cycle",
        "A ticket cannot be merged into one of its own descendants.",
      );
    }
  }

  const goneIds = new Set(merged.map((node) => node.id));
  const description =
    input.description ??
    [target.description, ...merged.map((node) => node.description)].join("\n\n");
  const dependsOn = [
    ...new Set(
      [target, ...merged]
        .flatMap((node) => node.dependsOn)
        .filter((id) => !goneIds.has(id) && id !== target.id),
    ),
  ];

  // A child of a folded ticket becomes a child of the survivor, after the
  // survivor's own children and in the order its tickets were folded.
  let tail = childrenOf(nodes, target.id).length;
  const adopted = new Map<string, number>();
  for (const source of merged) {
    for (const child of childrenOf(nodes, source.id)) adopted.set(child.id, tail++);
  }

  const next = nodes
    .filter((node) => !goneIds.has(node.id))
    .map((node) => {
      if (node.id === target.id) {
        return { ...node, title: input.title ?? node.title, description, dependsOn };
      }
      const ordinal = adopted.get(node.id);
      return ordinal === undefined ? node : { ...node, parentId: target.id, ordinal };
    });
  return normalizeTree(next);
}

/** Insert one node among a parent's children at `index`, shifting the rest. */
function spliceSibling(
  nodes: readonly SpecTicketNode[],
  node: SpecTicketNode,
  parentId: string | null,
  index: number | undefined,
): SpecTicketNode[] {
  const siblings = childrenOf(nodes, parentId);
  const position = Math.max(0, Math.min(index ?? siblings.length, siblings.length));
  const placed = { ...node, parentId, ordinal: position };
  const shifted = nodes.map((candidate) =>
    candidate.parentId === parentId && candidate.ordinal >= position
      ? { ...candidate, ordinal: candidate.ordinal + 1 }
      : candidate,
  );
  return [...shifted, placed];
}
