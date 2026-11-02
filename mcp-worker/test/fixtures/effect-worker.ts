// Actual worker and provider HTTPS servers in Node. DNS/dial routing is a test boundary.
import { createInterface } from "node:readline";
import { createServer, request } from "node:https";
import { createWorker } from "../../src/service.ts";
import { ZonePolicy } from "../../src/network.ts";
const lines = createInterface({ input: process.stdin });
const input = lines[Symbol.asyncIterator]();
const { tls, response = "normal" } = JSON.parse((await input.next()).value!);
let calls = 0,
  credentialRequests = 0;
const provider = createServer(tls, (req, res) => {
  let raw = "";
  req.on("data", (chunk: Buffer) => {
    raw += chunk;
  });
  req.on("end", () => {
    if (req.headers.authorization) credentialRequests++;
    const message = JSON.parse(raw);
    let result: unknown;
    if (message.method === "server/discover")
      result = {
        supportedVersions: ["2026-07-28"],
        capabilities: { tools: {} },
      };
    else if (message.method === "tools/call") {
      calls++;
      if (response === "hold") return;
      if (response === "malformed") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end("{broken");
        return;
      }
      const secret = String(req.headers.authorization ?? "");
      const text =
        response === "oversize"
          ? "x".repeat(1100000)
          : response === "normalized-limit"
            ? "x".repeat(550000)
            : `${secret} | ${secret.replace(/^Bearer /, "")}`;
      result = {
        content:
          response === "image"
            ? [{ type: "image", data: "AA==", mimeType: "image/png" }]
            : [{ type: "text", text }],
        structuredContent: { echo: text },
        isError: response === "error",
      };
    } else {
      res.writeHead(202);
      res.end();
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        jsonrpc: "2.0",
        id: message.id,
        result: { ...Object(result), resultType: "complete" },
      }),
    );
  });
});
await new Promise<void>((resolve) => provider.listen(0, "127.0.0.1", resolve));
const address = provider.address();
if (!address || typeof address === "string") throw Error("fixture address");
const zone = new ZonePolicy({
  id: "public",
  revision: "1",
  mode: "public",
  authorities: ["tools.example.test"],
  aliases: [],
  cidrs: [],
  denyCidrs: [],
});
const make = () =>
  createWorker({
    tls,
    zone,
    enabled: true,
    callers: [{ fingerprint: tls.fingerprint, role: "orchestrator" }],
    network: {
      resolve: async () => ["93.184.216.34"],
      request: (_url, options) =>
        request(new URL(`https://127.0.0.1:${address.port}/mcp`), {
          ...options,
          ca: tls.ca,
          servername: "localhost",
          lookup: (_host, _options, callback) => callback(null, "127.0.0.1", 4),
        }),
    },
  });
let worker = make();
const port = await worker.listen(0, "127.0.0.1");
const send = (value: unknown) =>
  process.stdout.write(`${JSON.stringify(value)}\n`);
send({ port, identity: worker.identity, node: process.versions.node });
for await (const command of { [Symbol.asyncIterator]: () => input }) {
  if (command === "stats") send({ calls, credentialRequests });
  else if (command === "restart") {
    await worker.close();
    worker = make();
    await worker.listen(port, "127.0.0.1");
    send({ identity: worker.identity });
  } else if (command === "close") {
    await worker.close();
    provider.closeAllConnections();
    await new Promise<void>((resolve) => provider.close(() => resolve()));
    lines.close();
    break;
  }
}
