import { afterEach, describe, expect, test, vi } from "vitest";

import { specBlockRenderAdapters } from "./block-renderers";
import { D2Client, type D2Worker } from "./d2-client";

interface WorkerMessage {
  id?: number;
  type: "compile" | "render";
}

type D2WorkerResponse = Parameters<Parameters<D2Worker["onMessage"]>[0]>[0];

class FakeWorker implements D2Worker {
  private messageHandler: ((message: D2WorkerResponse) => void) | null = null;
  private errorHandler: ((message: string) => void) | null = null;
  readonly posted: WorkerMessage[] = [];
  terminated = false;

  postMessage(message: WorkerMessage): void {
    this.posted.push(message);
  }

  terminate(): void {
    this.terminated = true;
  }

  onMessage(handler: Parameters<D2Worker["onMessage"]>[0]): void {
    this.messageHandler = handler;
  }

  onError(handler: Parameters<D2Worker["onError"]>[0]): void {
    this.errorHandler = handler;
  }

  emitMessage(data: D2WorkerResponse): void {
    this.messageHandler?.(data);
  }

  emitError(message: string): void {
    this.errorHandler?.(message);
  }
}

function compile(client: D2Client) {
  return client.compile({
    fs: { "index.d2": "A -> B" },
    inputPath: "index.d2",
    options: { layout: "dagre" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("D2Client", () => {
  test("an ID-free initialization error rejects readiness", async () => {
    const worker = new FakeWorker();
    const client = new D2Client(worker);
    const request = compile(client);

    worker.emitMessage({ type: "error", error: "WASM initialization failed" });

    await expect(request).rejects.toThrow("WASM initialization failed");
    expect(client.failed).toBe(true);
    expect(worker.terminated).toBe(true);
  });

  test("a fatal worker error rejects every pending request", async () => {
    const worker = new FakeWorker();
    const client = new D2Client(worker);
    worker.emitMessage({ type: "ready" });
    const first = compile(client);
    const second = compile(client);
    await vi.waitFor(() => expect(worker.posted).toHaveLength(2));

    worker.emitError("worker stopped");

    await expect(first).rejects.toThrow("worker stopped");
    await expect(second).rejects.toThrow("worker stopped");
  });

  test("the renderer drops a failed singleton and recovers with a new worker", async () => {
    const workers: FakeWorker[] = [];
    class RecoveringWorker extends FakeWorker {
      onmessage: ((event: { data: D2WorkerResponse }) => void) | null = null;
      onerror: ((event: { message: string }) => void) | null = null;

      constructor() {
        super();
        const attempt = workers.push(this);
        queueMicrotask(() => {
          if (attempt === 1) {
            this.emitBrowserMessage({
              type: "error",
              error: "temporary initialization failure",
            });
          } else {
            this.emitBrowserMessage({ type: "ready" });
          }
        });
      }

      override postMessage(message: WorkerMessage): void {
        super.postMessage(message);
        queueMicrotask(() => {
          if (message.type === "compile") {
            this.emitBrowserMessage({
              id: message.id,
              type: "result",
              data: { diagram: {}, renderOptions: {} },
            });
          } else {
            this.emitBrowserMessage({ id: message.id, type: "result", data: "<svg></svg>" });
          }
        });
      }

      private emitBrowserMessage(data: D2WorkerResponse): void {
        this.onmessage?.({ data });
      }
    }
    vi.stubGlobal("Worker", RecoveringWorker);

    await expect(specBlockRenderAdapters.d2.render("A -> B", "block-1")).rejects.toThrow(
      "temporary initialization failure",
    );
    await expect(specBlockRenderAdapters.d2.render("A -> B", "block-1")).resolves.toBe(
      "<svg></svg>",
    );
    expect(workers).toHaveLength(2);
  });
});
