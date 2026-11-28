import { getHeapStatistics } from "node:v8";
import { fullFormats } from "ajv-formats/dist/formats.js";
import { Ajv } from "ajv";
import { Ajv2020 } from "ajv/dist/2020.js";
import { object } from "./encoding.ts";
// No network loader, custom keyword, executable format, coercion or mutation.
if (getHeapStatistics().heap_size_limit > 128 * 1024 * 1024) process.exit(1);
process.stdout.write("ready\n");
let raw = "";
process.stdin.setEncoding("utf8");
for await (const part of process.stdin) {
  raw += String(part);
  if (raw.length > 1024 * 1024) process.exit(1);
}
try {
  const request: unknown = JSON.parse(raw);
  if (!object(request) || !Array.isArray(request.documents)) throw Error();
  let valid = true;
  for (const document of request.documents) {
    let nodes = 0;
    function guard(node: unknown, depth = 0) {
      if (++nodes > 10000 || depth > 64) throw Error();
      if (typeof node === "boolean") return;
      if (!object(node)) throw Error();
      if (
        node.$schema !== undefined &&
        ![
          "http://json-schema.org/draft-07/schema#",
          "http://json-schema.org/draft-07/schema",
          "https://json-schema.org/draft/2020-12/schema",
        ].includes(String(node.$schema))
      )
        throw Error();
      for (const key of ["$ref", "$dynamicRef", "$recursiveRef"])
        if (
          node[key] !== undefined &&
          (typeof node[key] !== "string" || !node[key].startsWith("#"))
        )
          throw Error();
      for (const key of [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
      ])
        if (object(node[key]))
          for (const schema of Object.values(node[key]))
            guard(schema, depth + 1);
      for (const key of ["allOf", "anyOf", "oneOf", "prefixItems"])
        if (Array.isArray(node[key]))
          for (const schema of node[key]) guard(schema, depth + 1);
      for (const key of [
        "items",
        "additionalProperties",
        "additionalItems",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "unevaluatedProperties",
        "unevaluatedItems",
      ])
        if (node[key] !== undefined) {
          if (Array.isArray(node[key]))
            for (const schema of node[key]) guard(schema, depth + 1);
          else guard(node[key], depth + 1);
        }
      if (object(node.dependencies))
        for (const schema of Object.values(node.dependencies))
          if (!Array.isArray(schema)) guard(schema, depth + 1);
    }
    guard(document);
    const schema = object(document) ? document : {};
    const Constructor =
      schema.$schema === "https://json-schema.org/draft/2020-12/schema"
        ? Ajv2020
        : Ajv;
    const ajv = new Constructor({
      strictSchema: true,
      strictTypes: false,
      strictTuples: false,
      strictRequired: false,
      allErrors: false,
      validateFormats: true,
      coerceTypes: false,
      useDefaults: false,
      removeAdditional: false,
      inlineRefs: false,
    });
    for (const [name, format] of Object.entries(fullFormats))
      ajv.addFormat(name, format);
    const check = ajv.compile(document);
    if ("value" in request && !check(request.value)) valid = false;
  }
  process.stdout.write(String(valid));
} catch {
  process.exitCode = 1;
}
