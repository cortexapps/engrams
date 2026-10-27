/** The variable paths a trigger's event offers, from the connector's curated
 * field schema (the bounded subset the catalog declares) and from a sample
 * payload. Both feed the inline `${{` completion: the schema names what an
 * event always carries, the sample shows what the last one carried. */

interface FieldSchema {
  type?: string;
  properties?: Record<string, FieldSchema>;
  items?: FieldSchema;
}

const MAX_DEPTH = 6;
const MAX_PATHS = 400;

/** Every dotted path under `prefix` the schema declares, parents included
 * (`event.raw.issue` and `event.raw.issue.title`). Arrays stop: a template
 * indexes them by hand. */
export function schemaPaths(schemaJson: string, prefix = "event.raw"): string[] {
  if (!schemaJson) return [];
  let schema: FieldSchema;
  try {
    schema = JSON.parse(schemaJson) as FieldSchema;
  } catch {
    return [];
  }
  const out: string[] = [];
  const walk = (node: FieldSchema, path: string, depth: number) => {
    if (out.length >= MAX_PATHS || depth > MAX_DEPTH) return;
    if (node.type !== "object" || !node.properties) return;
    for (const [key, child] of Object.entries(node.properties)) {
      const childPath = `${path}.${key}`;
      out.push(childPath);
      walk(child, childPath, depth + 1);
    }
  };
  walk(schema, prefix, 0);
  return out;
}

/** The static paths merged with the paths a preview knows a value for, in a
 * stable order: static first, then the previewed ones not already listed. */
export function mergePaths(
  statics: readonly string[],
  values: Readonly<Record<string, string>> | undefined,
): string[] {
  const seen = new Set(statics);
  const out = [...statics];
  for (const path of Object.keys(values ?? {})) {
    if (!seen.has(path)) {
      seen.add(path);
      out.push(path);
    }
  }
  return out;
}
