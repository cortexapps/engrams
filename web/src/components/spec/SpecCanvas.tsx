import { useEffect, useMemo } from "react";
import { Collaboration } from "@tiptap/extension-collaboration";
import { CollaborationCaret } from "@tiptap/extension-collaboration-caret";
import { EditorContent, useEditor } from "@tiptap/react";
import { SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { useAuth } from "@/auth/AuthProvider";
import { collaboratorColor } from "@/components/spec-mode/collaborator-colors";
import { Button } from "@/components/ui/button";
import { useIsMobile } from "@/hooks/use-mobile";
import { SpecPresence } from "./SpecPresence";
import { SpecSelectionBubbleMenu, type SpecSelectionActions } from "./SpecSelectionActions";
import { SpecBlockIterationProvider } from "./block-iteration";
import { specNodeExtensions } from "./extensions";
import "./spec-canvas.css";

export interface SpecConnection {
  doc: Y.Doc;
  provider: WebsocketProvider;
}

type WebSocketProviderOptions = NonNullable<ConstructorParameters<typeof WebsocketProvider>[3]>;

interface CreateSpecProviderOptions {
  connect?: boolean;
  location?: Pick<Location, "host" | "protocol">;
  WebSocketPolyfill?: WebSocketProviderOptions["WebSocketPolyfill"];
}

export function SpecCanvas({
  doc,
  provider,
  specId,
  revision,
  selectionActions,
}: {
  doc: Y.Doc;
  provider: WebsocketProvider;
  specId: string;
  revision: string;
  /** Supply this only when the current user can send selection actions. */
  selectionActions?: SpecSelectionActions;
}) {
  const { principal } = useAuth();
  const user = useMemo(
    () => ({
      name: principal.display_name || principal.email,
      color: collaboratorColor(principal.email),
    }),
    [principal.display_name, principal.email],
  );

  return (
    <SpecBlockIterationProvider specId={specId}>
      <ConnectedSpecCanvas
        connection={{ doc, provider }}
        user={user}
        specId={specId}
        revision={revision}
        selectionActions={selectionActions}
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
}: {
  connection: SpecConnection;
  user: { name: string; color: string };
  specId: string;
  revision: string;
  selectionActions?: SpecSelectionActions;
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
      {specDocument}
    </div>
  );
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
