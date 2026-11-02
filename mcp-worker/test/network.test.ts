import assert from "node:assert/strict";
import { test } from "node:test";
import { ZonePolicy, restrictedFetch } from "../src/network.ts";

const config = {
  id: "public",
  revision: "1",
  mode: "public" as const,
  authorities: ["tools.example"],
  aliases: [],
  cidrs: [],
  denyCidrs: ["203.0.113.0/24"],
  ca: undefined,
};
test("destination and all DNS answers are checked before any dial", async () => {
  let dials = 0;
  for (const answers of [
    ["127.0.0.1"],
    ["169.254.169.254"],
    ["::1"],
    ["::ffff:127.0.0.1"],
    ["8.8.8.8", "10.0.0.1"],
    ["8.8.8.8", "fd00::1"],
    ["203.0.113.1"],
  ]) {
    const fetch = restrictedFetch(new ZonePolicy(config), {
      resolve: async () => answers,
      request: () => {
        dials++;
        throw Error("dial");
      },
    });
    await assert.rejects(fetch("https://tools.example/mcp"));
  }
  const fetch = restrictedFetch(new ZonePolicy(config), {
    resolve: async () => ["8.8.8.8"],
    request: () => {
      dials++;
      throw Error("dial");
    },
  });
  for (const url of [
    "http://tools.example/mcp",
    "https://evil.example/mcp",
    "https://user@tools.example/mcp",
    "https://tools.example/mcp?q=1",
    "https://tools.example/mcp#x",
    "https://127.0.0.1/mcp",
    "https://tools.example:8443/mcp",
    "https://tools.example./mcp",
  ])
    await assert.rejects(fetch(url));
  assert.equal(dials, 0);
});
test("internal approval never admits permanent denies and policy digest covers authority", () => {
  const zone = new ZonePolicy({
    ...config,
    mode: "internal",
    cidrs: ["10.0.0.0/8"],
  });
  zone.checkAddresses(["10.1.2.3"]);
  assert.throws(() => zone.checkAddresses(["127.0.0.1"]));
  assert.throws(() => zone.checkAddresses(["169.254.169.254"]));
  assert.notEqual(
    zone.digest,
    new ZonePolicy({ ...config, authorities: ["other.example"] }).digest,
  );
});

test("real TLS preserves authority and pins one checked DNS answer on each dial", async (t) => {
  const { createServer, request } = await import("node:https");
  const { workerTls } = await import("./fixtures/worker-tls.ts");
  const tls = await workerTls();
  let host = "";
  let sni = "";
  let hits = 0;
  const server = createServer(tls, (req, res) => {
    hits++;
    host = req.headers.host ?? "";
    sni = "servername" in req.socket ? String(req.socket.servername) : "";
    res.end("ok");
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise<void>((resolve) => server.close(() => resolve())));
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  let resolutions = 0;
  let pinned = "";
  const zone = new ZonePolicy({ ...config, ca: tls.ca });
  const fetch = restrictedFetch(zone, {
    resolve: async () => {
      resolutions++;
      return resolutions === 1 ? ["8.8.8.8"] : ["127.0.0.1"];
    },
    request: (url, options) => {
      assert.equal(options.servername, "tools.example");
      return request(url, {
        ...options,
        port: address.port,
        lookup: (hostname, lookupOptions, callback) => {
          assert.ok(options.lookup);
          options.lookup(hostname, lookupOptions, (error, checked, family) => {
            assert.equal(error, null);
            pinned = String(checked);
            callback(null, "127.0.0.1", family);
          });
        },
      });
    },
  });
  assert.equal(await (await fetch("https://tools.example/mcp")).text(), "ok");
  assert.equal(pinned, "8.8.8.8");
  assert.equal(host, "tools.example");
  assert.equal(sni, "tools.example");
  await assert.rejects(fetch("https://tools.example/mcp"));
  assert.equal(hits, 1);
  assert.equal(resolutions, 2);
  for (const [ca, authority] of [
    [undefined, "tools.example"],
    [tls.ca, "other.example"],
  ] as const) {
    const bad = restrictedFetch(
      new ZonePolicy({ ...config, ca, authorities: [authority] }),
      {
        resolve: async () => ["8.8.8.8"],
        request: (url, options) =>
          request(url, {
            ...options,
            hostname: "127.0.0.1",
            port: address.port,
          }),
      },
    );
    await assert.rejects(bad(`https://${authority}/mcp`));
  }
  assert.equal(hits, 1);
});
test("real HTTPS refuses redirects and bounds decompressed bodies", async (t) => {
  const { createServer, request } = await import("node:https");
  const { gzipSync } = await import("node:zlib");
  const { workerTls } = await import("./fixtures/worker-tls.ts");
  const tls = await workerTls();
  let hits = 0;
  const server = createServer(tls, (req, res) => {
    hits++;
    if (req.url === "/redirect") {
      res.writeHead(302, { location: "https://tools.example/next" }).end();
      return;
    }
    res
      .writeHead(200, { "content-encoding": "gzip" })
      .end(gzipSync("x".repeat(2 * 1024 * 1024 + 1)));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise<void>((resolve) => server.close(() => resolve())));
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const fetch = restrictedFetch(new ZonePolicy({ ...config, ca: tls.ca }), {
    resolve: async () => ["8.8.8.8"],
    request: (url, options) =>
      request(url, { ...options, hostname: "127.0.0.1", port: address.port }),
  });
  await assert.rejects(fetch("https://tools.example/redirect"));
  assert.equal(hits, 1);
  await assert.rejects((await fetch("https://tools.example/large")).text());
  assert.equal(hits, 2);
});
test("permanent metadata and non-global IPv6 denies cannot be widened by an internal zone", () => {
  const zone = new ZonePolicy({
    ...config,
    mode: "internal",
    cidrs: ["fc00::/7"],
  });
  for (const ip of [
    "168.63.129.16",
    "fd00:ec2::254",
    "fd20:ce::254",
    "fec0::1",
    "3fff::1",
    "64:ff9b:1::1",
  ])
    assert.throws(() => zone.checkAddresses([ip]), ip);
});
test("internal CIDRs cannot grant public routes and defaults enter policy identity", () => {
  assert.throws(
    () =>
      new ZonePolicy({ ...config, mode: "internal", cidrs: ["8.8.8.0/24"] }),
  );
  assert.throws(
    () =>
      new ZonePolicy({ ...config, mode: "internal", cidrs: ["10.0.0.0/7"] }),
  );
  const a = new ZonePolicy(config);
  const b = new ZonePolicy({ ...config, limits: { execute: 31 } });
  assert.notEqual(a.digest, b.digest);
});

for (const fault of ["status", "header"] as const) {
  test(
    `invalid upstream ${fault} rejects only its operation`,
    { timeout: 3000 },
    async (t) => {
      const { createServer, request } = await import("node:https");
      const { workerTls } = await import("./fixtures/worker-tls.ts");
      const tls = await workerTls();
      let requests = 0;
      const server = createServer(tls, (_req, res) => {
        requests++;
        res
          .writeHead(requests === 1 && fault === "status" ? 600 : 200)
          .end("ok");
      });
      await new Promise<void>((resolve) =>
        server.listen(0, "127.0.0.1", resolve),
      );
      t.after(() => {
        server.closeAllConnections();
        server.close();
      });
      const address = server.address();
      assert.ok(address && typeof address !== "string");
      let conversions = 0;
      const fetch = restrictedFetch(new ZonePolicy({ ...config, ca: tls.ca }), {
        resolve: async () => ["8.8.8.8"],
        request: (url, options) => {
          const req = request(url, {
            ...options,
            hostname: "127.0.0.1",
            port: address.port,
          });
          req.prependOnceListener("response", (res) => {
            if (++conversions === 1 && fault === "header")
              res.headers["invalid\nname"] = "untrusted";
          });
          return req;
        },
      });
      await assert.rejects(fetch("https://tools.example/mcp"), {
        message: "response_invalid",
      });
      assert.equal(
        await (await fetch("https://tools.example/mcp")).text(),
        "ok",
      );
      assert.equal(requests, 2);
    },
  );
}
