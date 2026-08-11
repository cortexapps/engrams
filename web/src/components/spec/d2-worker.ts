interface GoRuntime {
  importObject: WebAssembly.Imports;
  run(instance: WebAssembly.Instance): Promise<void>;
}

interface D2WasmApi {
  compile(input: string): Promise<string>;
  render(input: string): Promise<string>;
}

interface D2WorkerGlobal {
  readonly location: Location;
  onmessage: ((event: MessageEvent<WorkerRequest>) => void) | null;
  postMessage(message: unknown): void;
  Go?: new () => GoRuntime;
  d2?: D2WasmApi;
}

interface WorkerRequest {
  id: number;
  type: "compile" | "render";
  data: unknown;
}

interface D2Response {
  data?: unknown;
  error?: { message?: string };
}

const workerGlobal = self as D2WorkerGlobal;

void initialize().catch((error: unknown) => {
  workerGlobal.postMessage({ type: "error", error: errorMessage(error) });
});

async function initialize(): Promise<void> {
  const runtimeUrl = new URL("/d2/wasm_exec.js", workerGlobal.location.href).href;
  await import(/* @vite-ignore */ runtimeUrl);
  if (!workerGlobal.Go) throw new Error("The D2 Go runtime did not load.");

  const go = new workerGlobal.Go();
  const wasmResponse = await fetch(new URL("/d2/d2.wasm", workerGlobal.location.href));
  if (!wasmResponse.ok) throw new Error(`The D2 WASM request failed: ${wasmResponse.status}`);
  const wasm = await WebAssembly.instantiate(await wasmResponse.arrayBuffer(), go.importObject);
  void go.run(wasm.instance);
  if (!workerGlobal.d2) throw new Error("The D2 WASM API did not start.");
  workerGlobal.onmessage = (event: MessageEvent<WorkerRequest>) => {
    void handleRequest(event.data);
  };
  workerGlobal.postMessage({ type: "ready" });
}

async function handleRequest(request: WorkerRequest): Promise<void> {
  try {
    const api = workerGlobal.d2;
    if (!api) throw new Error("The D2 WASM API is not ready.");
    if (request.type === "compile") {
      const response = parseResponse(await api.compile(JSON.stringify(request.data)));
      workerGlobal.postMessage({ id: request.id, type: "result", data: response });
      return;
    }

    const response = parseResponse(await api.render(JSON.stringify(request.data)));
    if (typeof response !== "string") throw new Error("The D2 renderer returned invalid data.");
    const bytes = Uint8Array.from(atob(response), (value) => value.charCodeAt(0));
    workerGlobal.postMessage({
      id: request.id,
      type: "result",
      data: new TextDecoder().decode(bytes),
    });
  } catch (error: unknown) {
    workerGlobal.postMessage({ id: request.id, type: "error", error: errorMessage(error) });
  }
}

function parseResponse(value: string): unknown {
  const response = JSON.parse(value) as D2Response;
  if (response.error) throw new Error(response.error.message ?? "The D2 renderer failed.");
  return response.data;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
