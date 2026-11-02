// Actual Node worker fixture. Admission receipts are test inputs, not database proof.
import { createInterface } from "node:readline";
import { createServer } from "node:https";
import { createWorker } from "../../src/service.ts";
import { ZonePolicy } from "../../src/network.ts";

const lines = createInterface({ input: process.stdin });
const input = lines[Symbol.asyncIterator]();
const first = await input.next();
const tls = JSON.parse(first.value!);
let providerDials = 0;
const makeWorker = () =>
  createWorker({
    tls,
    zone: new ZonePolicy({
      id: "public",
      revision: "1",
      mode: "public",
      authorities: ["tools.example"],
      aliases: [],
      cidrs: [],
      denyCidrs: [],
    }),
    callers: [{ fingerprint: tls.fingerprint, role: "orchestrator" }],
    enabled: true,
    network: {
      resolve: async () => {
        providerDials++;
        throw new Error("fixture has no provider");
      },
      request: () => {
        providerDials++;
        throw new Error("fixture has no provider");
      },
    },
  });
let worker = makeWorker();
const port = await worker.listen(0, "127.0.0.1");
const send = (value: unknown) =>
  process.stdout.write(`${JSON.stringify(value)}\n`);
send({ port, identity: worker.identity, node: process.versions.node });
let replacement: ReturnType<typeof createServer> | undefined;
let replacementBytes = 0;
let replacementRequests = 0;
for await (const line of { [Symbol.asyncIterator]: () => input }) {
  if (line === "restart") {
    await worker.close();
    worker = makeWorker();
    await worker.listen(port, "127.0.0.1");
    send({ identity: worker.identity });
  } else if (line === "replace") {
    await worker.close();
    replacement = createServer(
      { ...tls, requestCert: true, rejectUnauthorized: true },
      (req, res) => {
        replacementRequests++;
        req.on("data", (chunk: Buffer) => {
          replacementBytes += chunk.length;
        });
        req.on("end", () => {
          res.writeHead(500);
          res.end();
        });
      },
    );
    await new Promise<void>((resolve) =>
      replacement!.listen(port, "127.0.0.1", resolve),
    );
    send({ replaced: true });
  } else if (line === "stats") {
    send({ providerDials, replacementBytes, replacementRequests });
  } else if (line === "close") {
    if (replacement) {
      replacement.closeAllConnections();
      await new Promise<void>((resolve) => replacement!.close(() => resolve()));
    } else await worker.close();
    lines.close();
    break;
  }
}
