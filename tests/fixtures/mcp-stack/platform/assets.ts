import { constants } from "node:fs";
import { chmod, copyFile, mkdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  check,
  childEnvironment,
  command,
  hash,
  put,
  readOwned,
  verifyPin,
  type Pin,
} from "./owned.ts";
import { Engine } from "./engine.ts";
import { REPO, type Inputs, type ImagePin } from "./preflight.ts";

/** Bind raw archive manifest and config bytes to the two independent image pins. */
export function verifyArchiveIdentity(
  image: ImagePin,
  raw: Uint8Array,
  config: Uint8Array,
) {
  const manifest = JSON.parse(Buffer.from(raw).toString());
  check(
    manifest.schemaVersion === 2 &&
      !manifest.manifests &&
      manifest.config?.digest === image.id &&
      `sha256:${hash(raw)}` === image.manifestDigest,
    "archive_image_identity",
  );
  check(`sha256:${hash(config)}` === image.id, "archive_config_identity");
}

export async function stageAssets(
  inputs: Inputs,
  root: string,
  engine: Engine,
) {
  const directory = join(root, "assets"),
    bundles = join(root, "bundles");
  const sourceFile = async (path: string) => {
    const committed = await command(
      "/usr/bin/git",
      ["-C", REPO, "show", `${inputs.sourceCommit}:${path}`],
      REPO,
      childEnvironment(root),
    );
    check(
      committed.equals(await readFile(join(REPO, path))),
      "asset_source_changed",
    );
    return committed;
  };
  await mkdir(directory, { mode: 0o700 });
  await mkdir(bundles, { mode: 0o700 });
  async function copy(
    name: string,
    source: Pin,
    executable = false,
  ): Promise<Pin> {
    await verifyPin(source);
    const path = join(directory, name);
    await copyFile(source.path, path, constants.COPYFILE_EXCL);
    await chmod(path, executable ? 0o700 : 0o600);
    const result = { path, sha256: source.sha256 };
    await verifyPin(result);
    return result;
  }
  const binaries = { ...inputs.binaries };
  for (const [name, pin] of Object.entries(inputs.binaries))
    binaries[name as keyof typeof binaries] = await copy(
      name,
      pin,
      ![
        "kernel",
        "agentdBundle",
        "sentinelBundle",
        "guestToolsBundle",
      ].includes(name),
    );
  const proxy = await copy("proxy", inputs.proxy, true);
  const nativeProbe = inputs.nativeProbe
    ? await copy("nativeProbe", inputs.nativeProbe, true)
    : undefined;
  const firecracker = inputs.firecracker
    ? await copy("firecracker", inputs.firecracker, true)
    : undefined;
  // MaterializeImage uses this exact built-in shim, with its transport substitution.
  const source = (
    await sourceFile("crates/engram-rootfs-materializer/src/inject.rs")
  ).toString();
  const match = /pub const DEFAULT_INIT_SHIM: &str = r#"([\s\S]*?)"#;/.exec(
    source,
  );
  check(
    match &&
      Buffer.from(match[1]!).equals(await readOwned(binaries.guestInit.path)),
    "production_init_shim",
  );
  const artifacts = {
    coordinator: await copy(
      "coordinator.tar",
      inputs.images.coordinator.archive,
    ),
    orchestrator: await copy(
      "orchestrator.tar",
      inputs.images.orchestrator.archive,
    ),
    worker: await copy("worker.tar", inputs.images.worker.archive),
    fixture: await copy("fixture.tar", inputs.images.fixture.archive),
    guestImage: await copy("guest.tar", inputs.images.guest.archive),
    hostAgent: binaries.hostAgent,
    kernel: binaries.kernel,
    helper: binaries.helper,
    claudeHarness: binaries.claudeHarness,
    codexHarness: binaries.codexHarness,
    modelCli: binaries.modelCli,
  };
  for (const [role, image] of Object.entries(inputs.images)) {
    await verifyPin(image.archive);
    const stage = `verify-${role.toLowerCase()}`;
    const id = await engine.create(
      stage,
      inputs.images.guard.id,
      [
        "--network",
        "none",
        "--user",
        `${process.getuid?.()}:${process.getgid?.()}`,
        "--read-only",
        "--mount",
        `type=bind,src=${image.archive.path},dst=/input/image.tar,readonly`,
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=256m,mode=1777",
      ],
      ["infinity"],
      "/bin/sleep",
    );
    try {
      const raw = await engine.call([
        "exec",
        id,
        "/usr/bin/skopeo",
        "--tmpdir",
        "/tmp",
        "inspect",
        "--raw",
        "oci-archive:/input/image.tar",
      ]);
      const config = await engine.call([
        "exec",
        id,
        "/usr/bin/skopeo",
        "--tmpdir",
        "/tmp",
        "inspect",
        "--raw",
        "--config",
        "oci-archive:/input/image.tar",
      ]);
      verifyArchiveIdentity(image, raw, config);
    } finally {
      await engine.remove(stage);
    }
    await verifyPin(image.archive);
  }
  const stamp: Record<string, string> = {};
  for (const [logical, input] of [
    ["agentd", binaries.agentdBundle],
    ["sentinel", binaries.sentinelBundle],
    ["guest-tools", binaries.guestToolsBundle],
  ] as const) {
    await copyFile(
      input.path,
      join(bundles, `${input.sha256}.squashfs`),
      constants.COPYFILE_EXCL,
    );
    await chmod(join(bundles, `${input.sha256}.squashfs`), 0o600);
    stamp[logical] = input.sha256;
  }
  const recipes = join(directory, "recipes");
  await mkdir(recipes, { mode: 0o700 });
  await put(
    join(recipes, "_pack.sh"),
    await sourceFile("deploy/bundles/_pack.sh"),
  );
  for (const name of ["claude", "codex"] as const) {
    const path = join(recipes, `harness-${name}`);
    await mkdir(path, { mode: 0o700 });
    await put(
      join(path, "build.sh"),
      await sourceFile(`deploy/bundles/harness-${name}/build.sh`),
    );
  }
  const packed: Partial<Record<"claude" | "codex", Pin>> = {};
  for (const harness of ["claude", "codex"] as const) {
    const tree = join(directory, `${harness}-tree`);
    await mkdir(tree, { mode: 0o700 });
    for (const [name, source] of [
      [
        "harness",
        harness === "claude" ? binaries.claudeHarness : binaries.codexHarness,
      ],
      [harness, binaries.modelCli],
      ["engram-mcp-model-fixture", binaries.modelCli],
    ] as const) {
      await copyFile(source.path, join(tree, name), constants.COPYFILE_EXCL);
      await chmod(join(tree, name), 0o755);
    }
    await put(
      join(tree, "harness.toml"),
      await sourceFile(`deploy/harness-${harness}/harness.toml`),
    );
    const output = join(directory, `${harness}.squashfs`);
    const role = `pack-${harness}`;
    const id = await engine.create(
      role,
      inputs.images.guard.id,
      [
        "--network",
        "none",
        "--read-only",
        "--user",
        `${process.getuid?.()}:${process.getgid?.()}`,
        "--mount",
        `type=bind,src=${directory},dst=${directory}`,
        "--mount",
        `type=bind,src=${recipes},dst=/recipes,readonly`,
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=64m",
      ],
      [`/recipes/harness-${harness}/build.sh`, tree, output],
      "/bin/bash",
    );
    check(
      (await engine.call(["wait", id], undefined, 30000)).toString().trim() ===
        "0",
      "bundle_pack",
    );
    await engine.remove(role);
    const bytes = await readFile(output);
    const sha256 = hash(bytes);
    await chmod(output, 0o600);
    const pin = { path: output, sha256 };
    await verifyPin(pin);
    await copyFile(
      output,
      join(bundles, `${sha256}.squashfs`),
      constants.COPYFILE_EXCL,
    );
    await chmod(join(bundles, `${sha256}.squashfs`), 0o600);
    stamp[`harness-${harness}`] = sha256;
    packed[harness] = pin;
  }
  // This is the existing host bundle stamp (logical name -> content hash), not a guest tool manifest.
  await put(join(bundles, "current.json"), JSON.stringify(stamp));
  check(packed.claude && packed.codex, "bundle_set");
  return {
    binaries,
    proxy,
    nativeProbe,
    firecracker,
    artifacts: {
      ...artifacts,
      claudeBundle: packed.claude,
      codexBundle: packed.codex,
    },
    bundles,
    stamp,
  };
}

/** The immutable archive, config blob and registry manifest must be one image. */
export async function publishGuest(
  inputs: Inputs,
  root: string,
  engine: Engine,
) {
  await verifyPin(inputs.images.guest.archive);
  const guard = engine.guardIds.get("coordinator");
  check(guard, "registry_namespace");
  const role = "image-copy";
  const id = await engine.create(
    role,
    inputs.images.guard.id,
    [
      "--network",
      `container:${guard}`,
      "--user",
      `${process.getuid?.()}:${process.getgid?.()}`,
      "--read-only",
      "--mount",
      `type=bind,src=${inputs.images.guest.archive.path},dst=/input/guest.tar,readonly`,
      "--tmpfs",
      "/tmp:rw,nosuid,nodev,size=256m,mode=1777",
    ],
    ["infinity"],
    "/bin/sleep",
  );
  try {
    const archive = "oci-archive:/input/guest.tar";
    const manifest = await engine.call([
      "exec",
      id,
      "/usr/bin/skopeo",
      "--tmpdir",
      "/tmp",
      "inspect",
      "--raw",
      archive,
    ]);
    const config = await engine.call([
      "exec",
      id,
      "/usr/bin/skopeo",
      "--tmpdir",
      "/tmp",
      "inspect",
      "--raw",
      "--config",
      archive,
    ]);
    verifyArchiveIdentity(inputs.images.guest, manifest, config);
    const uri = `127.0.0.1:${inputs.registryPort}/${engine.runId}/guest`;
    await engine.call(
      [
        "exec",
        id,
        "/usr/bin/skopeo",
        "--tmpdir",
        "/tmp",
        "copy",
        "--preserve-digests",
        "--dest-tls-verify=false",
        archive,
        `docker://${uri}:fixture`,
      ],
      undefined,
      120000,
    );
    const actual = await engine.call([
      "exec",
      id,
      "/usr/bin/skopeo",
      "--tmpdir",
      "/tmp",
      "inspect",
      "--tls-verify=false",
      "--raw",
      `docker://${uri}@${inputs.images.guest.manifestDigest}`,
    ]);
    check(hash(actual) === hash(manifest), "registry_manifest_changed");
    await verifyPin(inputs.images.guest.archive);
    await put(
      join(root, "image-provenance.json"),
      JSON.stringify({
        version: 1,
        archiveSha256: inputs.images.guest.archive.sha256,
        manifestDigest: inputs.images.guest.manifestDigest,
        configDigest: inputs.images.guest.id,
        imageUri: `${uri}@${inputs.images.guest.manifestDigest}`,
      }),
    );
    return `${uri}@${inputs.images.guest.manifestDigest}`;
  } finally {
    await engine.remove(role);
  }
}
