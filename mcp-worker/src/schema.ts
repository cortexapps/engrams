import childProcess from "node:child_process";
let activeChildren = 0;
export async function validateSchemas(
  documents: unknown[],
  value?: unknown,
  signal?: AbortSignal,
): Promise<boolean> {
  signal?.throwIfAborted();
  const payload = JSON.stringify({
    documents,
    ...(value === undefined ? {} : { value }),
  });
  if (documents.length > 384 || Buffer.byteLength(payload) > 1024 * 1024)
    throw Error("schema_limit");
  if (activeChildren >= 4) throw Error("schema_capacity");
  const file = new URL(
    import.meta.url.endsWith(".ts") ? "./schema-child.ts" : "./schema-child.js",
    import.meta.url,
  );
  activeChildren++;
  return new Promise((resolve, reject) => {
    const child = childProcess.spawn(
      process.execPath,
      ["--max-old-space-size=125", "--max-semi-space-size=1", file.pathname],
      { env: {}, stdio: ["pipe", "pipe", "ignore"], cwd: "/" },
    );
    let output = "";
    let failure: string | undefined;
    const kill = (reason: string) => {
      failure = reason;
      child.kill("SIGKILL");
    };
    let ready = false;
    let timer = setTimeout(() => kill("schema_bootstrap_deadline"), 5000);
    const abort = () => kill("schema_aborted");
    signal?.addEventListener("abort", abort, { once: true });
    child.stdout.on("data", (data: Buffer) => {
      output += data.toString();
      if (!ready && output.startsWith("ready\n")) {
        ready = true;
        output = output.slice(6);
        clearTimeout(timer);
        timer = setTimeout(() => kill("schema_deadline"), 1000);
        // Start the hard schema budget before untrusted bytes enter the child.
        child.stdin.end(payload);
      }
      if (output.length > 1024) kill("schema_output_limit");
    });
    child.stdin.on("error", () => {});
    child.once("error", () => {
      failure = "schema_process";
    });
    child.once("close", (code) => {
      activeChildren--;
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
      if (failure) reject(Error(failure));
      else if (code !== 0 || !["true", "false"].includes(output))
        reject(Error("schema_invalid"));
      else resolve(output === "true");
    });
  });
}
