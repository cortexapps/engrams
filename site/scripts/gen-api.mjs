#!/usr/bin/env node
// Generate the per-service API reference from the public Connect RPC protos at
// build time. The pages land in src/content/docs/docs/api/ (gitignored) and the
// sidebar picks them up. Only names and types are emitted, never the proto
// comments: those are engineering notes for the repository, not product docs.

import { mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const PROTO_DIR = join(ROOT, "..", "crates", "engram-protocol", "proto", "engram", "app", "v1");
const OUT_DIR = join(ROOT, "src", "content", "docs", "docs", "api");
const SKIP_FILES = new Set(["spec.proto"]);
const REPO_PROTO_URL = "https://github.com/cortexapps/engrams/blob/main/crates/engram-protocol/proto/engram/app/v1";

// Strip line and block comments.
function stripComments(src) {
  return src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
}

/** Tokenize into words, punctuation, and braces. */
function tokenize(src) {
  const re = /[A-Za-z_][A-Za-z0-9_.]*|<|>|,|=|;|\{|\}|\(|\)|\[|\]|"[^"]*"|-?\d+/g;
  return src.match(re) ?? [];
}

/** Parse one proto file into { services, messages, enums }. */
function parseProto(src) {
  const t = tokenize(stripComments(src));
  let i = 0;
  const peek = () => t[i];
  const next = () => t[i++];
  const expect = (x) => {
    const v = next();
    if (v !== x) throw new Error(`expected ${x} got ${v}`);
  };
  const skipBlock = () => {
    let depth = 0;
    do {
      const v = next();
      if (v === "{") depth++;
      if (v === "}") depth--;
      if (v === undefined) return;
    } while (depth > 0);
  };
  const skipBrackets = () => {
    let depth = 0;
    do {
      const v = next();
      if (v === "[") depth++;
      if (v === "]") depth--;
      if (v === undefined) return;
    } while (depth > 0);
  };
  const skipStatement = () => {
    while (i < t.length && peek() !== ";" && peek() !== "{") next();
    if (peek() === "{") skipBlock();
    else next();
  };

  const services = [];
  const messages = [];
  const enums = [];

  function parseEnum(prefix) {
    const name = next();
    expect("{");
    const values = [];
    while (peek() !== "}") {
      const v = next();
      if (v === "option" || v === "reserved") {
        skipStatement();
        continue;
      }
      expect("=");
      const num = next();
      if (peek() === "[") skipBrackets();
      expect(";");
      values.push({ name: v, number: num });
    }
    expect("}");
    enums.push({ name: prefix + name, values });
  }

  function parseType() {
    let ty = next();
    if (ty === "map") {
      expect("<");
      const k = next();
      expect(",");
      const v = parseType();
      expect(">");
      ty = `map<${k}, ${v}>`;
    }
    return ty;
  }

  function parseMessage(prefix) {
    const name = next();
    const full = prefix + name;
    expect("{");
    const fields = [];
    while (peek() !== "}") {
      const v = peek();
      if (v === "message") {
        next();
        parseMessage(full + ".");
        continue;
      }
      if (v === "enum") {
        next();
        parseEnum(full + ".");
        continue;
      }
      if (v === "oneof") {
        next();
        const oneof = next();
        expect("{");
        while (peek() !== "}") {
          if (peek() === "option") {
            skipStatement();
            continue;
          }
          const ty = parseType();
          const fname = next();
          expect("=");
          next();
          if (peek() === "[") skipBrackets();
          expect(";");
          fields.push({ name: fname, type: ty, label: `one of ${oneof}` });
        }
        expect("}");
        continue;
      }
      if (v === "option" || v === "reserved" || v === "extensions") {
        next();
        skipStatement();
        continue;
      }
      let label = "";
      if (v === "repeated" || v === "optional") {
        label = next();
      }
      const ty = parseType();
      const fname = next();
      expect("=");
      next();
      if (peek() === "[") skipBrackets();
      expect(";");
      fields.push({ name: fname, type: ty, label });
    }
    expect("}");
    messages.push({ name: full, fields });
  }

  function parseService() {
    const name = next();
    expect("{");
    const rpcs = [];
    while (peek() !== "}") {
      const v = next();
      if (v === "option") {
        skipStatement();
        continue;
      }
      if (v !== "rpc") throw new Error(`unexpected ${v} in service ${name}`);
      const rpc = next();
      expect("(");
      let reqStream = false;
      if (peek() === "stream") {
        next();
        reqStream = true;
      }
      const req = next();
      expect(")");
      expect("returns");
      expect("(");
      let resStream = false;
      if (peek() === "stream") {
        next();
        resStream = true;
      }
      const res = next();
      expect(")");
      if (peek() === "{") skipBlock();
      else expect(";");
      rpcs.push({ name: rpc, req, res, reqStream, resStream });
    }
    expect("}");
    services.push({ name, rpcs });
  }

  while (i < t.length) {
    const v = next();
    if (v === "service") parseService();
    else if (v === "message") parseMessage("");
    else if (v === "enum") parseEnum("");
    else if (v === "syntax" || v === "package" || v === "import" || v === "option") skipStatement();
    else if (v === "{") skipBlock();
  }
  return { services, messages, enums };
}

const SCALARS = new Set([
  "string", "bool", "bytes", "int32", "int64", "uint32", "uint64", "sint32", "sint64",
  "fixed32", "fixed64", "sfixed32", "sfixed64", "float", "double",
]);

function typeCell(ty, label, known) {
  const base = ty.replace(/^\./, "");
  let cell;
  if (SCALARS.has(base) || base.startsWith("map<")) cell = `\`${base}\``;
  else if (known.has(base)) cell = `[\`${base}\`](#${anchor(base)})`;
  else cell = `\`${base}\``;
  if (label === "repeated") cell = `repeated ${cell}`;
  else if (label === "optional") cell = `${cell} (optional)`;
  else if (label) cell = `${cell} (${label})`;
  return cell;
}

const anchor = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, "");

/** Collect the messages a service's RPCs reference, transitively, within one file. */
function reachable(service, messages) {
  const byName = new Map(messages.map((m) => [m.name, m]));
  const seen = new Set();
  const stack = [];
  for (const r of service.rpcs) stack.push(r.req, r.res);
  while (stack.length) {
    const n = stack.pop();
    if (seen.has(n) || !byName.has(n)) continue;
    seen.add(n);
    for (const f of byName.get(n).fields) {
      const inner = f.type.match(/^map<[^,]+,\s*(.+)>$/)?.[1] ?? f.type;
      stack.push(inner.replace(/^\./, ""));
    }
  }
  return messages.filter((m) => seen.has(m.name));
}

rmSync(OUT_DIR, { recursive: true, force: true });
mkdirSync(OUT_DIR, { recursive: true });

const files = readdirSync(PROTO_DIR).filter((f) => f.endsWith(".proto") && !SKIP_FILES.has(f)).sort();
let pages = 0;
const index = [];

for (const file of files) {
  const src = readFileSync(join(PROTO_DIR, file), "utf8");
  let parsed;
  try {
    parsed = parseProto(src);
  } catch (e) {
    console.error(`gen-api: ${file}: ${e.message}`);
    process.exit(1);
  }
  const enumNames = new Set(parsed.enums.map((e) => e.name));
  for (const svc of parsed.services) {
    const msgs = reachable(svc, parsed.messages);
    const known = new Set([...msgs.map((m) => m.name), ...enumNames]);
    const usedEnums = parsed.enums.filter((e) =>
      msgs.some((m) => m.fields.some((f) => f.type.replace(/^\./, "") === e.name)),
    );
    const slug = svc.name.replace(/Service$/, "").replace(/([a-z])([A-Z])/g, "$1-$2").toLowerCase();
    const lines = [];
    lines.push("---");
    lines.push(`title: ${svc.name}`);
    lines.push(`description: The ${svc.name} methods and their request and response messages.`);
    lines.push("---");
    lines.push("");
    lines.push(`Generated from [\`${file}\`](${REPO_PROTO_URL}/${file}). Every method is`);
    lines.push(`\`POST /rpc/engram.app.v1.${svc.name}/<Method>\` with a JSON or protobuf body and an`);
    lines.push("`x-api-key` header.");
    lines.push("");
    lines.push("## Methods");
    lines.push("");
    lines.push("| Method | Request | Response |");
    lines.push("|---|---|---|");
    for (const r of svc.rpcs) {
      const req = `[\`${r.req}\`](#${anchor(r.req)})${r.reqStream ? " (stream)" : ""}`;
      const res = `[\`${r.res}\`](#${anchor(r.res)})${r.resStream ? " (stream)" : ""}`;
      lines.push(`| \`${r.name}\` | ${req} | ${res} |`);
    }
    lines.push("");
    lines.push("## Messages");
    lines.push("");
    for (const m of msgs.sort((a, b) => a.name.localeCompare(b.name))) {
      lines.push(`### ${m.name}`);
      lines.push("");
      if (m.fields.length === 0) {
        lines.push("No fields.");
      } else {
        lines.push("| Field | Type |");
        lines.push("|---|---|");
        for (const f of m.fields) lines.push(`| \`${f.name}\` | ${typeCell(f.type, f.label, known)} |`);
      }
      lines.push("");
    }
    if (usedEnums.length) {
      lines.push("## Enums");
      lines.push("");
      for (const e of usedEnums) {
        lines.push(`### ${e.name}`);
        lines.push("");
        lines.push("| Value | Number |");
        lines.push("|---|---|");
        for (const v of e.values) lines.push(`| \`${v.name}\` | ${v.number} |`);
        lines.push("");
      }
    }
    writeFileSync(join(OUT_DIR, `${slug}.md`), lines.join("\n"));
    index.push({ name: svc.name, slug, rpcs: svc.rpcs.length });
    pages += 1;
  }
}

const idx = [];
idx.push("---");
idx.push("title: API reference");
idx.push("description: Every Connect RPC service, generated from the proto files.");
idx.push("sidebar:");
idx.push("  order: 0");
idx.push("---");
idx.push("");
idx.push("One page per service, generated at build time from the proto files in the repository.");
idx.push("Every method is `POST /rpc/engram.app.v1.<Service>/<Method>` with a JSON or protobuf body");
idx.push("and an `x-api-key` header; the [API overview](../reference/api/) covers authentication,");
idx.push("the routes outside RPC, and client generation.");
idx.push("");
idx.push("| Service | Methods |");
idx.push("|---|---|");
for (const s of index.sort((a, b) => a.name.localeCompare(b.name))) idx.push(`| [${s.name}](${s.slug}/) | ${s.rpcs} |`);
idx.push("");
writeFileSync(join(OUT_DIR, "index.md"), idx.join("\n"));
console.log(`gen-api: ${pages} services from ${files.length} proto files → src/content/docs/docs/api/`);
