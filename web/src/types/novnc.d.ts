// Type shim for `@novnc/novnc` (1.7.0) — the package ships ESM with no
// bundled `.d.ts`, and there is no `@types/novnc__novnc` on the registry.
// The package's `exports` map points the bare specifier at `core/rfb.js`
// (whose default export is the `RFB` class), so the module name here is the
// bare specifier — NOT `@novnc/novnc/lib/rfb` (no `lib/` dir exists in 1.7).
//
// Only the slice of RFB's surface that BrowserPane drives is declared; noVNC's
// own docs/API.md is the source of truth for the rest.
declare module "@novnc/novnc" {
  export interface RFBOptions {
    /** RFB credentials (password/username/target) — unused: the relay is auth-gated. */
    credentials?: { username?: string; password?: string; target?: string };
    /** Whether to request a shared session (default true). */
    shared?: boolean;
    /** UltraVNC repeater ID. */
    repeaterID?: string;
    /** WebSocket subprotocols passed to the underlying WebSocket. */
    wsProtocols?: string[];
  }

  /**
   * noVNC RFB client. Renders into `target` and speaks RFB over the WebSocket
   * URL (or a pre-opened WebSocket/RTCDataChannel). Emits `connect`,
   * `disconnect`, `credentialsrequired`, `securityfailure`, etc. as DOM events.
   */
  export default class RFB extends EventTarget {
    constructor(
      target: HTMLElement,
      urlOrChannel: string | WebSocket | RTCDataChannel,
      options?: RFBOptions,
    );

    /** When true the viewer is read-only (no input forwarded). */
    viewOnly: boolean;
    /** Scale the remote framebuffer to fit the container. */
    scaleViewport: boolean;
    /** Ask the server to resize its framebuffer to the container (RANDR). */
    resizeSession: boolean;
    /** Clip the viewport to the container instead of growing it. */
    clipViewport: boolean;

    /** Disconnect and tear down the session. */
    disconnect(): void;
  }
}
