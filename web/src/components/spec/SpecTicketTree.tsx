/**
 * The ticket tree (ADR 0114 D6, R29, R39-R40, mock 2l).
 *
 * After publish the canvas becomes this. It must read as a tree editor and
 * not as a chat transcript, because direct manipulation is the primary verb
 * here: retitle, edit, split, merge, reorder, re-parent, add, delete.
 *
 * A move applies locally first, using the same pure functions the server runs
 * (`@engrams/spec-document`), and the server's reply then replaces the tree.
 * That is why a drop lands at once and still cannot drift: one implementation
 * decides what a move means, on both sides. A refused move puts the tree back
 * exactly as it was and says why.
 *
 * A drag always has a keyboard equal. The grip is a button, and the arrow keys
 * do the same four things a drag does.
 */

import {
  backlinkBody,
  mergeTickets,
  moveTicket,
  orderedTree,
  type SpecTicketNode,
} from "@engrams/spec-document";
import {
  ChevronRight,
  GripVertical,
  Merge,
  Plus,
  Split,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { SpecTicketSyncBadge } from "@/pages/specs/SpecTicketSyncBadge";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import type { SpecTicket, SpecTicketCommand, SpecTicketTree } from "@/hooks/useSpecTickets";
import "./spec-ticket-tree.css";

export interface SpecTicketTreeProps {
  tree: SpecTicketTree;
  /** Runs one command and resolves with the tree the server stored. */
  onCommand: (command: SpecTicketCommand) => Promise<SpecTicketTree>;
  /** Replaces the tree the editor shows, after a local move or a reply. */
  onTree: (tree: SpecTicketTree) => void;
}

export function SpecTicketTree({ tree, onCommand, onTree }: SpecTicketTreeProps) {
  const [selected, setSelected] = useState<string[]>([]);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  // The row being dragged also lives in a ref. The drop must know it whether
  // or not a re-render landed between `dragstart` and `drop`; state alone
  // would make a fast drag depend on render timing.
  const dragged = useRef<string | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);
  const live = useRef(tree);
  live.current = tree;

  useEffect(() => {
    // A ticket that a merge or a delete removed cannot stay selected.
    const present = new Set(tree.tickets.map((ticket) => ticket.id));
    setSelected((current) => current.filter((id) => present.has(id)));
  }, [tree]);

  /** Show `optimistic` at once, then keep whatever the server stored. */
  async function run(command: SpecTicketCommand, optimistic?: SpecTicketTree): Promise<void> {
    const before = live.current;
    if (optimistic) onTree(optimistic);
    setError(null);
    setPending(true);
    try {
      onTree(await onCommand(command));
    } catch (failure) {
      onTree(before);
      setError(failure instanceof Error ? failure.message : "The change did not land.");
    } finally {
      setPending(false);
    }
  }

  /**
   * Apply a tree operation locally, then send it.
   *
   * The tree refuses some operations itself — a drop onto one's own child, a
   * merge into one's own descendant. A refusal is an expected outcome and not
   * a fault, so it becomes a message and nothing is sent. Returns false when
   * the tree refused, so a caller can leave its own state alone.
   */
  function attempt(
    change: (nodes: SpecTicketNode[]) => SpecTicketNode[],
    command: SpecTicketCommand,
  ): boolean {
    let optimistic: SpecTicketTree;
    try {
      optimistic = applyLocally(tree, change);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "That change is not allowed.");
      return false;
    }
    void run(command, optimistic);
    return true;
  }

  function move(id: string, parentId: string | null, index?: number): void {
    const where = { id, parentId, ...(index === undefined ? {} : { index }) };
    attempt((nodes) => moveTicket(nodes, where), { kind: "move", ...where });
  }

  function merge(): void {
    const [targetId, ...sourceIds] = selected;
    if (targetId === undefined || sourceIds.length === 0) return;
    // The selection survives a refusal, so the person can pick again.
    const sent = attempt((nodes) => mergeTickets(nodes, { targetId, sourceIds }), {
      kind: "merge",
      targetId,
      sourceIds,
    });
    if (sent) setSelected([targetId]);
  }

  const rows = tree.tickets;
  const byId = new Map(rows.map((ticket) => [ticket.id, ticket]));
  const carriedQuestions = new Set(
    rows.flatMap((ticket) => ticket.openQuestions.map((question) => question.id)),
  );

  return (
    <div className="spec-ticket-tree">
      <header className="spec-ticket-bar">
        <span className="spec-ticket-count">Tickets · {rows.length}</span>
        <div className="spec-ticket-bar-actions">
          <Button
            variant="outline"
            size="sm"
            disabled={selected.length < 2 || pending}
            onClick={merge}
          >
            <Merge aria-hidden /> Merge {selected.length > 1 ? selected.length : ""}
          </Button>
          <Button
            variant="outline"
            size="sm"
            disabled={pending}
            onClick={() =>
              void run({
                kind: "add",
                parentId: null,
                title: "New ticket",
                body: "",
                sectionId: tree.sections[0]?.id ?? "",
              })
            }
          >
            <Plus aria-hidden /> Add ticket
          </Button>
        </div>
      </header>

      {error && (
        <p className="spec-ticket-error" role="alert">
          {error}
        </p>
      )}

      <ol className="spec-ticket-rows" aria-label="Ticket tree">
        {rows.map((ticket) => (
          <TicketRow
            key={ticket.id}
            ticket={ticket}
            selected={selected.includes(ticket.id)}
            editing={editingId === ticket.id}
            dragging={draggingId === ticket.id}
            dropTarget={dropTarget === ticket.id}
            disabled={pending}
            sections={tree.sections}
            onSelect={(checked) =>
              setSelected((current) =>
                checked ? [...current, ticket.id] : current.filter((id) => id !== ticket.id),
              )
            }
            onEdit={() => setEditingId(editingId === ticket.id ? null : ticket.id)}
            onSave={(changes) => {
              setEditingId(null);
              void run({ kind: "update", id: ticket.id, ...changes });
            }}
            onDelete={() => void run({ kind: "delete", id: ticket.id })}
            onSplit={() =>
              void run({
                kind: "split",
                id: ticket.id,
                parts: [
                  { title: ticket.title, body: ticket.body },
                  { title: `${ticket.title} (part 2)`, body: "" },
                ],
              })
            }
            onDragStart={() => {
              dragged.current = ticket.id;
              setDraggingId(ticket.id);
            }}
            onDragEnd={() => {
              dragged.current = null;
              setDraggingId(null);
              setDropTarget(null);
            }}
            onDragOver={() => setDropTarget(ticket.id)}
            onDrop={() => {
              const source = dragged.current;
              dragged.current = null;
              setDraggingId(null);
              setDropTarget(null);
              if (source && source !== ticket.id) move(source, ticket.id);
            }}
            onNest={() => {
              const previous = previousSibling(rows, ticket);
              if (previous) move(ticket.id, previous.id);
            }}
            onOutdent={() => {
              const parent = ticket.parentId === null ? null : byId.get(ticket.parentId);
              if (parent) move(ticket.id, parent.parentId);
            }}
            onShift={(offset) => {
              const target = ticket.ordinal + offset;
              if (target >= 0) move(ticket.id, ticket.parentId, target);
            }}
          />
        ))}
      </ol>

      <p className="spec-ticket-hint">Add a ticket, or drag a row onto another to nest it.</p>

      {tree.unattachedQuestions.length > 0 && (
        <section className="spec-ticket-orphans" aria-label="Open questions with no ticket">
          <h3>
            <TriangleAlert aria-hidden /> {tree.unattachedQuestions.length} open question
            {tree.unattachedQuestions.length === 1 ? "" : "s"} no ticket covers
          </h3>
          <ul>
            {tree.unattachedQuestions.map((question) => (
              <li key={question.id}>
                {question.text}
                {carriedQuestions.has(question.id) ? null : (
                  <span className="spec-ticket-orphan-section">
                    §{sectionTitle(tree, question.sectionId)}
                  </span>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

interface TicketRowProps {
  ticket: SpecTicket;
  selected: boolean;
  editing: boolean;
  dragging: boolean;
  dropTarget: boolean;
  disabled: boolean;
  sections: Array<{ id: string; title: string }>;
  onSelect: (checked: boolean) => void;
  onEdit: () => void;
  onSave: (changes: { title: string; body: string; sectionId: string }) => void;
  onDelete: () => void;
  onSplit: () => void;
  onDragStart: () => void;
  onDragEnd: () => void;
  onDragOver: () => void;
  onDrop: () => void;
  onNest: () => void;
  onOutdent: () => void;
  onShift: (offset: number) => void;
}

function TicketRow(props: TicketRowProps) {
  const { ticket } = props;
  return (
    <li
      className="spec-ticket-row"
      data-depth={ticket.depth}
      data-selected={props.selected ? "true" : undefined}
      data-dragging={props.dragging ? "true" : undefined}
      data-drop-target={props.dropTarget ? "true" : undefined}
      style={{ "--ticket-depth": ticket.depth } as React.CSSProperties}
      onDragOver={(event) => {
        event.preventDefault();
        props.onDragOver();
      }}
      onDrop={(event) => {
        event.preventDefault();
        props.onDrop();
      }}
    >
      <div className="spec-ticket-head">
        <button
          type="button"
          className="spec-ticket-grip"
          aria-label={`Reorder ${ticket.title}`}
          draggable
          disabled={props.disabled}
          onDragStart={props.onDragStart}
          onDragEnd={props.onDragEnd}
          onKeyDown={(event) => {
            const handled = keyboardMove(event.key, props);
            if (handled) event.preventDefault();
          }}
        >
          <GripVertical aria-hidden />
        </button>
        <input
          type="checkbox"
          className="spec-ticket-select"
          checked={props.selected}
          disabled={props.disabled}
          aria-label={`Select ${ticket.title}`}
          onChange={(event) => props.onSelect(event.target.checked)}
        />
        <button
          type="button"
          className="spec-ticket-title"
          aria-expanded={props.editing}
          onClick={props.onEdit}
        >
          <ChevronRight aria-hidden className="spec-ticket-caret" />
          {ticket.title}
        </button>
        <a className="spec-ticket-backlink" href={ticket.backlink.href}>
          §{ticket.backlink.sectionTitle}
        </a>
        {ticket.openQuestions.length > 0 && (
          <Badge
            variant="outline"
            className="spec-ticket-flag"
            title={ticket.openQuestions.map((question) => question.text).join("\n")}
          >
            ⚑{ticket.openQuestions.length}
          </Badge>
        )}
        {ticket.linearId ? (
          <span className="spec-ticket-linear">{ticket.linearId}</span>
        ) : (
          <SpecTicketSyncBadge state={ticket.syncState === "draft" ? "none" : ticket.syncState} />
        )}
        <div className="spec-ticket-row-actions">
          <Button
            variant="ghost"
            size="sm"
            disabled={props.disabled}
            aria-label={`Split ${ticket.title}`}
            onClick={props.onSplit}
          >
            <Split aria-hidden />
          </Button>
          <Button
            variant="ghost"
            size="sm"
            disabled={props.disabled}
            aria-label={`Delete ${ticket.title}`}
            onClick={props.onDelete}
          >
            <Trash2 aria-hidden />
          </Button>
        </div>
      </div>
      {props.editing && (
        <TicketEditor ticket={ticket} sections={props.sections} onSave={props.onSave} />
      )}
    </li>
  );
}

function TicketEditor({
  ticket,
  sections,
  onSave,
}: {
  ticket: SpecTicket;
  sections: Array<{ id: string; title: string }>;
  onSave: (changes: { title: string; body: string; sectionId: string }) => void;
}) {
  const [title, setTitle] = useState(ticket.title);
  const [body, setBody] = useState(ticket.body);
  const [sectionId, setSectionId] = useState(ticket.backlink.sectionId);
  return (
    <form
      className="spec-ticket-editor"
      onSubmit={(event) => {
        event.preventDefault();
        onSave({ title, body, sectionId });
      }}
    >
      <label>
        Title
        <Input value={title} onChange={(event) => setTitle(event.target.value)} />
      </label>
      <label>
        Description
        <Textarea rows={4} value={body} onChange={(event) => setBody(event.target.value)} />
      </label>
      <label>
        Backlink
        <select value={sectionId} onChange={(event) => setSectionId(event.target.value)}>
          {sections.map((section) => (
            <option key={section.id} value={section.id}>
              §{section.title}
            </option>
          ))}
        </select>
      </label>
      <Button type="submit" size="sm">
        Save ticket
      </Button>
    </form>
  );
}

/** The keyboard equal of a drag: nest, outdent, and reorder. */
function keyboardMove(
  key: string,
  props: Pick<TicketRowProps, "onNest" | "onOutdent" | "onShift">,
): boolean {
  switch (key) {
    case "ArrowRight":
      props.onNest();
      return true;
    case "ArrowLeft":
      props.onOutdent();
      return true;
    case "ArrowUp":
      props.onShift(-1);
      return true;
    case "ArrowDown":
      props.onShift(1);
      return true;
    default:
      return false;
  }
}

function previousSibling(rows: SpecTicket[], ticket: SpecTicket): SpecTicket | undefined {
  return rows.find(
    (candidate) =>
      candidate.parentId === ticket.parentId && candidate.ordinal === ticket.ordinal - 1,
  );
}

function sectionTitle(tree: SpecTicketTree, sectionId: string): string {
  return tree.sections.find((section) => section.id === sectionId)?.title ?? sectionId;
}

/**
 * Run one tree operation locally and rebuild the view the server would send.
 * Depths and question attachment are derived, exactly as the server derives
 * them, so an optimistic tree and the stored one agree before the reply.
 */
export function applyLocally(
  tree: SpecTicketTree,
  change: (nodes: SpecTicketNode[]) => SpecTicketNode[],
): SpecTicketTree {
  const byId = new Map(tree.tickets.map((ticket) => [ticket.id, ticket]));
  const questions = allQuestions(tree);
  const next = orderedTree(change(tree.tickets.map(toNode)));
  const depths = new Map<string, number>();
  const tickets = next.map((node) => {
    const parentDepth = node.parentId === null ? -1 : (depths.get(node.parentId) ?? -1);
    depths.set(node.id, parentDepth + 1);
    const existing = byId.get(node.id);
    return {
      id: node.id,
      parentId: node.parentId,
      ordinal: node.ordinal,
      depth: parentDepth + 1,
      title: node.title,
      body: backlinkBody(node.description),
      description: node.description,
      backlink: existing?.backlink ?? {
        sectionId: node.sectionId,
        sectionTitle: sectionTitle(tree, node.sectionId),
        href: "",
      },
      dependsOn: node.dependsOn,
      syncState: node.syncState,
      linearId: node.linearId,
      syncError: node.syncError,
      // Derived from the section, exactly as the server derives it (R29).
      openQuestions: questions.filter((question) => question.sectionId === node.sectionId),
    } satisfies SpecTicket;
  });
  const covered = new Set(tickets.map((ticket) => ticket.backlink.sectionId));
  return {
    ...tree,
    tickets,
    unattachedQuestions: questions.filter((question) => !covered.has(question.sectionId)),
  };
}

function allQuestions(tree: SpecTicketTree) {
  const seen = new Map<string, SpecTicketTree["unattachedQuestions"][number]>();
  for (const question of tree.unattachedQuestions) seen.set(question.id, question);
  for (const ticket of tree.tickets) {
    for (const question of ticket.openQuestions) seen.set(question.id, question);
  }
  return [...seen.values()];
}

function toNode(ticket: SpecTicket): SpecTicketNode {
  return {
    id: ticket.id,
    parentId: ticket.parentId,
    ordinal: ticket.ordinal,
    title: ticket.title,
    description: ticket.description,
    sectionId: ticket.backlink.sectionId,
    dependsOn: ticket.dependsOn,
    syncState: ticket.syncState,
    linearId: ticket.linearId,
    syncError: ticket.syncError,
  };
}
