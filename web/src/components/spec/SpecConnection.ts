import { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

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

export function createSpecConnection(specId: string): SpecConnection {
  const doc = new Y.Doc();
  return { doc, provider: createSpecProvider(specId, doc) };
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
