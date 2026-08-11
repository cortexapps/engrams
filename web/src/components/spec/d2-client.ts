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

export interface D2Worker {
  postMessage(message: unknown): void;
  terminate(): void;
  onMessage(handler: (message: WorkerResponse) => void): void;
  onError(handler: (message: string) => void): void;
}

/** D2's package worker uses dynamic code. This client uses our static Dagre worker. */
export class D2Client {
  private readonly worker: D2Worker;
  private readonly pending = new Map<number, PendingRequest>();
  private readonly ready: Promise<void>;
  private rejectReady: ((error: Error) => void) | null = null;
  private fatalError: Error | null = null;
  private nextId = 1;

  constructor(worker: D2Worker = createD2Worker()) {
    this.worker = worker;
    this.ready = new Promise<void>((resolve, reject) => {
      this.rejectReady = reject;
      this.worker.onMessage((message) => {
        if (message.type === "ready") {
          if (this.fatalError) return;
          resolve();
          return;
        }
        if (message.type === "error" && message.id === undefined) {
          this.fail(new Error(message.error ?? "The D2 worker failed to start."));
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
      });
      this.worker.onError((message) => {
        this.fail(new Error(message || "The D2 worker failed to start."));
      });
    });
  }

  get failed(): boolean {
    return this.fatalError !== null;
  }

  async compile(input: CompileRequest): Promise<CompileResponse> {
    return (await this.send("compile", input)) as CompileResponse;
  }

  async render(diagram: Diagram, options: RenderOptions): Promise<string> {
    return (await this.send("render", { diagram, options })) as string;
  }

  private async send(type: RequestType, data: unknown): Promise<unknown> {
    await this.ready;
    if (this.fatalError) throw this.fatalError;
    const id = this.nextId;
    this.nextId += 1;
    const result = new Promise<unknown>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
    });
    try {
      this.worker.postMessage({ id, type, data });
    } catch (error) {
      this.fail(error instanceof Error ? error : new Error(String(error)));
    }
    return result;
  }

  private fail(error: Error): void {
    if (this.fatalError) return;
    this.fatalError = error;
    this.rejectReady?.(error);
    this.rejectReady = null;
    for (const request of this.pending.values()) request.reject(error);
    this.pending.clear();
    this.worker.terminate();
  }
}

function createD2Worker(): D2Worker {
  const worker = new Worker("/d2/d2-worker.js", { type: "module" });
  return {
    postMessage: (message) => worker.postMessage(message),
    terminate: () => worker.terminate(),
    onMessage: (handler) => {
      worker.onmessage = (event: MessageEvent<WorkerResponse>) => handler(event.data);
    },
    onError: (handler) => {
      worker.onerror = (event) => handler(event.message);
    },
  };
}
