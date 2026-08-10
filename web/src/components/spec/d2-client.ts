import type { CompileRequest, CompileResponse, Diagram, RenderOptions } from "@terrastruct/d2";

type RequestType = "compile" | "render";

interface WorkerResponse {
  id?: number;
  type: "ready" | "result" | "error";
  data?: unknown;
  error?: string;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
}

/** D2's package worker uses dynamic code. This client uses our static Dagre worker. */
export class D2Client {
  private readonly worker = new Worker("/d2/d2-worker.js", {
    type: "module",
  });
  private readonly pending = new Map<number, PendingRequest>();
  private readonly ready: Promise<void>;
  private nextId = 1;

  constructor() {
    this.ready = new Promise<void>((resolve, reject) => {
      this.worker.onmessage = (event: MessageEvent<WorkerResponse>) => {
        const message = event.data;
        if (message.type === "ready") {
          resolve();
          return;
        }
        if (message.id === undefined) return;
        const request = this.pending.get(message.id);
        if (!request) return;
        this.pending.delete(message.id);
        if (message.type === "error") {
          request.reject(new Error(message.error ?? "The D2 worker failed."));
        } else {
          request.resolve(message.data);
        }
      };
      this.worker.onerror = (event) => {
        reject(new Error(event.message || "The D2 worker failed to start."));
      };
    });
  }

  async compile(input: CompileRequest): Promise<CompileResponse> {
    return (await this.send("compile", input)) as CompileResponse;
  }

  async render(diagram: Diagram, options: RenderOptions): Promise<string> {
    return (await this.send("render", { diagram, options })) as string;
  }

  private async send(type: RequestType, data: unknown): Promise<unknown> {
    await this.ready;
    const id = this.nextId;
    this.nextId += 1;
    const result = new Promise<unknown>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
    });
    this.worker.postMessage({ id, type, data });
    return result;
  }
}
