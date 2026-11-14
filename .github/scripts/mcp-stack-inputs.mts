import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { lstat, open, realpath } from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  check,
  ownedPath,
  PlatformFailure,
  privateRoot,
  put,
  readOwned,
  record,
  type Pin,
} from "../../tests/fixtures/mcp-stack/platform/owned.ts";
import {
  BINARIES,
  IMAGES,
  parseInputs,
  type Inputs,
} from "../../tests/fixtures/mcp-stack/platform/preflight.ts";

function object(value: unknown): Record<string, unknown> {
  check(value !== null && typeof value === "object" && !Array.isArray(value), "object");
  return value as Record<string, unknown>;
}

async function pin(value: unknown): Promise<Pin> {
  const source = record(value, ["path"]);
  check(typeof source.path === "string", "artifact_path");
  const path = source.path;
  await ownedPath(path);
  check((await realpath(path)) === path, "artifact_path");
  const namedBefore = await lstat(path, { bigint: true });
  check(namedBefore.isFile(), "artifact_identity");
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const before = await file.stat({ bigint: true });
    check(
      before.isFile() &&
        before.dev === namedBefore.dev &&
        before.ino === namedBefore.ino &&
        before.uid === BigInt(process.getuid?.() ?? -1) &&
        before.nlink === 1n &&
        before.size > 0n,
      "artifact_identity",
    );
    const digest = createHash("sha256");
    let size = 0n;
    for await (const bytes of file.createReadStream({ autoClose: false })) {
      digest.update(bytes);
      size += BigInt(bytes.length);
    }
    const after = await file.stat({ bigint: true });
    await ownedPath(path);
    check((await realpath(path)) === path, "artifact_path");
    const named = await lstat(path, { bigint: true });
    for (const state of [after, named])
      check(
        state.isFile() &&
          state.dev === before.dev &&
          state.ino === before.ino &&
          state.uid === before.uid &&
          state.mode === before.mode &&
          state.nlink === 1n &&
          state.size === before.size &&
          state.mtimeNs === before.mtimeNs &&
          state.ctimeNs === before.ctimeNs,
        "artifact_changed",
      );
    check(size === before.size, "artifact_changed");
    return { path, sha256: digest.digest("hex") };
  } finally {
    await file.close();
  }
}

/** Hash staged files, validate the complete Inputs object, then create its private manifest. */
export async function materializeInputs(
  backend: Inputs["backend"],
  template: string,
  output: string,
): Promise<Inputs> {
  check(backend === "firecracker" || backend === "vz", "backend");
  await privateRoot(dirname(output));
  await ownedPath(output, true);
  const source = object(JSON.parse((await readOwned(template, 1 << 20)).toString("utf8")));
  const binaries = { ...object(source.binaries) };
  for (const key of BINARIES) binaries[key] = await pin(binaries[key]);
  const images = { ...object(source.images) };
  for (const key of [...IMAGES, "hostAgent"]) {
    if (!Object.hasOwn(images, key)) continue;
    const image = object(images[key]);
    images[key] = { ...image, archive: await pin(image.archive) };
  }
  const final: Record<string, unknown> = {
    ...source,
    binaries,
    images,
    proxy: await pin(source.proxy),
  };
  for (const key of ["firecracker", "nativeProbe"] as const)
    if (Object.hasOwn(source, key)) final[key] = await pin(source[key]);
  const inputs = parseInputs(final, backend);
  await put(output, JSON.stringify(inputs, null, 2) + "\n");
  return inputs;
}

export function cli(args: string[]): {
  backend: Inputs["backend"];
  template: string;
  output: string;
} {
  check(
    args.length === 6 &&
      args[0] === "--backend" &&
      ["firecracker", "vz"].includes(args[1]!) &&
      args[2] === "--template" &&
      args[4] === "--output",
    "arguments",
  );
  for (const path of [args[3]!, args[5]!])
    check(isAbsolute(path) && resolve(path) === path, "arguments_path");
  return { backend: args[1] as Inputs["backend"], template: args[3]!, output: args[5]! };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  try {
    const args = cli(process.argv.slice(2));
    await materializeInputs(args.backend, args.template, args.output);
  } catch (error) {
    console.error(error instanceof PlatformFailure ? error.message : "INVALID_CONTEXT:inputs");
    process.exitCode = 1;
  }
}
