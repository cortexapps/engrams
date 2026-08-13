import { useEffect, useMemo, useState } from "react";
import { Collaboration } from "@tiptap/extension-collaboration";
import { CollaborationCaret } from "@tiptap/extension-collaboration-caret";
import { EditorContent, useEditor } from "@tiptap/react";
import {
  SPEC_FRAGMENT_NAME,
  SPEC_NOTES_ARCHIVED_AT_KEY,
  SPEC_NOTES_FRAGMENT_NAME,
  SPEC_NOTES_STATE_NAME,
} from "@engrams/spec-document";
import { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { useAuth } from "@/auth/AuthProvider";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useIsMobile } from "@/hooks/use-mobile";
import { SpecNotesPane } from "./SpecNotesPane";
import { SpecPresence } from "./SpecPresence";
import { SpecSelectionBubbleMenu, type SpecSelectionActions } from "./SpecSelectionActions";
import { SpecBlockIterationProvider } from "./block-iteration";
import { specNodeExtensions } from "./extensions";
import { readSectionTitles, SpecSectionTitleProvider } from "./section-titles";
import "./spec-canvas.css";

interface SpecConnection {
  doc: Y.Doc;
  provider: WebsocketProvider;
}

type WebSocketProviderOptions = NonNullable<ConstructorParameters<typeof WebsocketProvider>[3]>;

interface CreateSpecProviderOptions {
  connect?: boolean;
  location?: Pick<Location, "host" | "protocol">;
  WebSocketPolyfill?: WebSocketProviderOptions["WebSocketPolyfill"];
}

export interface SpecNotesStageActions {
  /** Close the talk-it-through stage. Omit it for a viewer who cannot. */
  onDistill: () => void;
  distilling?: boolean;
  distillError?: string | null;
}

export function SpecCanvas({
  specId,
  revision,
  selectionActions,
  notesActions,
}: {
  specId: string;
  revision: string;
  /** Supply this only when the current user can send selection actions. */
  selectionActions?: SpecSelectionActions;
  notesActions?: SpecNotesStageActions;
}) {
  const { principal } = useAuth();
  const [connection, setConnection] = useState<SpecConnection | null>(null);
  const [synced, setSynced] = useState(false);
  const user = useMemo(
    () => ({
      name: principal.display_name || principal.email,
      color: presenceColor(principal.email),
    }),
    [principal.display_name, principal.email],
  );

  useEffect(() => {
    const doc = new Y.Doc();
    const provider = createSpecProvider(specId, doc);
    const onSync = (isSynced: boolean) => setSynced(isSynced);
    provider.on("sync", onSync);
    setConnection({ doc, provider });
    return () => {
      provider.off("sync", onSync);
      provider.destroy();
      doc.destroy();
      setConnection(null);
      setSynced(false);
    };
  }, [specId]);

  if (!connection || !synced) {
    return (
      <div className="spec-canvas-loading" aria-label="Loading collaborative spec">
        <Skeleton className="h-7 w-2/5" />
        <Skeleton className="h-4 w-full" />
        <Skeleton className="h-4 w-5/6" />
      </div>
    );
  }

  return (
    <SpecBlockIterationProvider specId={specId}>
      <ConnectedSpecCanvas
        connection={connection}
        user={user}
        specId={specId}
        revision={revision}
        selectionActions={selectionActions}
        notesActions={notesActions}
      />
    </SpecBlockIterationProvider>
  );
}

/**
 * The canvas over an established connection.
 *
 * A small screen is read-and-resolve (ADR 0114 D4, R51): the WYSIWYG belongs to
 * the desktop, so the editor mounts read-only and the writing controls go away.
 * The connection is untouched, so the document still changes as others type.
 */
export function ConnectedSpecCanvas({
  connection,
  user,
  specId,
  revision,
  selectionActions,
  notesActions,
}: {
  connection: SpecConnection;
  user: { name: string; color: string };
  specId: string;
  revision: string;
  selectionActions?: SpecSelectionActions;
  notesActions?: SpecNotesStageActions;
}) {
  const extensions = useMemo(
    () => [
      ...specNodeExtensions,
      Collaboration.configure({
        document: connection.doc,
        field: SPEC_FRAGMENT_NAME,
      }),
      CollaborationCaret.configure({ provider: connection.provider, user }),
    ],
    [connection.doc, connection.provider, user],
  );
  const readOnly = useIsMobile();
  const editor = useEditor({
    extensions,
    immediatelyRender: false,
    editable: !readOnly,
    editorProps: {
      attributes: {
        class: "spec-canvas-editor",
        "aria-label": "Collaborative spec document",
      },
    },
  });
  // The width can change under a mounted editor (a rotation, a resized window).
  useEffect(() => {
    editor?.setEditable(!readOnly);
  }, [editor, readOnly]);

  const notesStage = useNotesStage(connection.doc);
  const sectionTitles = useMemo(
    () => (editor ? readSectionTitles(editor.state.doc) : new Map<string, string>()),
    // The tags follow a retitled heading, so this re-reads on every revision.
    [editor, editor?.state.doc],
  );

  if (!editor) return null;
  const specDocument = (
    <>
      {selectionActions && !readOnly && (
        <SpecSelectionBubbleMenu
          editor={editor}
          doc={connection.doc}
          specId={specId}
          revision={revision}
          actions={selectionActions}
        />
      )}
      <EditorContent editor={editor} />
    </>
  );
  const notesPane = notesStage.present && (
    <SpecSectionTitleProvider titles={sectionTitles}>
      <SpecNotesPane
        doc={connection.doc}
        provider={connection.provider}
        user={user}
        archived={notesStage.archived}
        readOnly={readOnly}
        {...(notesActions === undefined || notesStage.archived
          ? {}
          : {
              onDistill: notesActions.onDistill,
              distilling: notesActions.distilling ?? false,
              distillError: notesActions.distillError ?? null,
            })}
      />
    </SpecSectionTitleProvider>
  );
  return (
    <div className="spec-canvas-shell">
      <div className="spec-canvas-bar">
        {readOnly ? (
          <p className="spec-canvas-read-only">Read-only on a small screen. The text stays live.</p>
        ) : (
          <div className="spec-canvas-tools" role="toolbar" aria-label="Spec formatting">
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => editor.chain().focus().setNode("paragraph").run()}
            >
              Body
            </Button>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => editor.chain().focus().setNode("heading", { level: 3 }).run()}
            >
              H3
            </Button>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => editor.chain().focus().setNode("codeBlock").run()}
            >
              Code
            </Button>
          </div>
        )}
        <SpecPresence awareness={connection.provider.awareness} />
      </div>
      {!notesStage.present && specDocument}
      {notesStage.present && (
        // While the notes are live the canvas leads with them: the agent's pen
        // is down, so the spec is the second tab, not the first (R21).
        <Tabs defaultValue={notesStage.archived ? "spec" : "notes"} className="spec-canvas-stage">
          <TabsList aria-label="Canvas view">
            <TabsTrigger value="notes">
              {notesStage.archived ? "Notes archive" : "Working notes"}
            </TabsTrigger>
            <TabsTrigger value="spec">Spec</TabsTrigger>
          </TabsList>
          <TabsContent value="notes">{notesPane}</TabsContent>
          <TabsContent value="spec">{specDocument}</TabsContent>
        </Tabs>
      )}
    </div>
  );
}

export interface SpecNotesStage {
  /** True once the agent opened the notes. */
  present: boolean;
  /** True after distillation. The pane is then a read-only archive (R23). */
  archived: boolean;
}

/**
 * The stage state, read from the live document.
 *
 * Both facts are plain Yjs, so the pane appears and collapses without a fetch,
 * and the notes editor mounts only when the server really holds notes: an empty
 * fragment would let the editor push a placeholder cluster into the document.
 */
export function readNotesStage(doc: Y.Doc): SpecNotesStage {
  const archivedAt = doc.getMap(SPEC_NOTES_STATE_NAME).get(SPEC_NOTES_ARCHIVED_AT_KEY);
  return {
    present: doc.getXmlFragment(SPEC_NOTES_FRAGMENT_NAME).length > 0,
    archived: typeof archivedAt === "string" && archivedAt.length > 0,
  };
}

function useNotesStage(doc: Y.Doc): SpecNotesStage {
  const [stage, setStage] = useState<SpecNotesStage>(() => readNotesStage(doc));
  useEffect(() => {
    const read = () => setStage(readNotesStage(doc));
    read();
    doc.on("update", read);
    return () => doc.off("update", read);
  }, [doc]);
  return stage;
}

export function createSpecProvider(
  specId: string,
  doc: Y.Doc,
  options: CreateSpecProviderOptions = {},
): WebsocketProvider {
  const providerOptions: WebSocketProviderOptions = {
    connect: options.connect ?? true,
    params: { clientId: String(doc.clientID) },
    ...(options.WebSocketPolyfill ? { WebSocketPolyfill: options.WebSocketPolyfill } : {}),
  };
  return new WebsocketProvider(
    specSocketBase(specId, options.location ?? window.location),
    "sync",
    doc,
    providerOptions,
  );
}

function specSocketBase(specId: string, location: Pick<Location, "host" | "protocol">): string {
  const protocol = location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${location.host}/api/v1/specs/${encodeURIComponent(specId)}`;
}

function presenceColor(seed: string): string {
  let hash = 0;
  for (const character of seed) hash = (hash * 31 + character.charCodeAt(0)) >>> 0;
  const palette = ["#2563eb", "#7c3aed", "#c2410c", "#0f766e", "#be123c", "#4f46e5"];
  return palette[hash % palette.length]!;
}
