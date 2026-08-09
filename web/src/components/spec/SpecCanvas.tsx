import { useEffect, useMemo, useState } from "react";
import { Collaboration } from "@tiptap/extension-collaboration";
import { CollaborationCaret } from "@tiptap/extension-collaboration-caret";
import { EditorContent, useEditor } from "@tiptap/react";
import { SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { useAuth } from "@/auth/AuthProvider";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { SpecPresence } from "./SpecPresence";
import { specNodeExtensions } from "./extensions";
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

export function SpecCanvas({ specId }: { specId: string }) {
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

  return <ConnectedSpecCanvas connection={connection} user={user} />;
}

function ConnectedSpecCanvas({
  connection,
  user,
}: {
  connection: SpecConnection;
  user: { name: string; color: string };
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
  const editor = useEditor({
    extensions,
    immediatelyRender: false,
    editorProps: {
      attributes: {
        class: "spec-canvas-editor",
        "aria-label": "Collaborative spec document",
      },
    },
  });

  if (!editor) return null;
  return (
    <div className="spec-canvas-shell">
      <div className="spec-canvas-bar">
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
        <SpecPresence awareness={connection.provider.awareness} />
      </div>
      <EditorContent editor={editor} />
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

function presenceColor(seed: string): string {
  let hash = 0;
  for (const character of seed) hash = (hash * 31 + character.charCodeAt(0)) >>> 0;
  const palette = ["#2563eb", "#7c3aed", "#c2410c", "#0f766e", "#be123c", "#4f46e5"];
  return palette[hash % palette.length]!;
}
