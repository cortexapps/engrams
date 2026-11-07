import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  access,
  chmod,
  chown,
  cp,
  link,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { cli, materializeInputs } from "../mcp-stack-inputs.mts";
import {
  BINARIES,
  IMAGES,
  parseInputs,
  REPO,
  type Inputs,
} from "../../../tests/fixtures/mcp-stack/platform/preflight.ts";

type PathPin = { path: string };
type Template = Omit<Inputs, "binaries" | "images" | "proxy" | "firecracker" | "nativeProbe"> & {
  binaries: Record<(typeof BINARIES)[number], PathPin>;
  images: Record<string, { archive: PathPin; id: string; manifestDigest: string }>;
  proxy: PathPin;
  firecracker?: PathPin;
  nativeProbe?: PathPin;
};
async function fixture(
  backend: Inputs["backend"],
  run: (f: {
    root: string;
    template: Template;
    source: string;
    output: string;
    save: () => Promise<void>;
  }) => Promise<void>,
) {
  const root = await mkdtemp(join(await realpath(tmpdir()), "mcp-inputs-"));
  try {
    await chmod(root, 0o700);
    const staged = async (name: string): Promise<PathPin> => {
      const path = join(root, name);
      await writeFile(path, `staged-${name}\n`, { mode: 0o600, flag: "wx" });
      return { path };
    };
    const binaries = Object.fromEntries(
      await Promise.all(BINARIES.map(async (key) => [key, await staged(key)])),
    ) as Template["binaries"];
    const images: Template["images"] = {};
    for (const key of [...IMAGES, ...(backend === "firecracker" ? ["hostAgent"] : [])])
      images[key] = {
        archive: await staged(`image-${key}`),
        id: `sha256:${"c".repeat(64)}`,
        manifestDigest: `sha256:${"d".repeat(64)}`,
      };
    const template: Template = {
      version: 1,
      backend,
      architecture: backend === "vz" ? "arm64" : "amd64",
      sourceCommit: "b".repeat(40),
      engineSocket: join(root, "engine.sock"),
      engineConfig: join(root, "engine-config"),
      engineId: "fixture-engine-identity",
      registryPort: 24567,
      subnet: "10.80.1.0/24",
      nativeHostIp: "192.168.50.2",
      binaries,
      images,
      proxy: await staged("proxy"),
      ...(backend === "vz"
        ? { nativeProbe: await staged("native-probe") }
        : { firecracker: await staged("firecracker") }),
    };
    const source = join(root, "template.json"),
      output = join(root, "inputs.json");
    const save = () => writeFile(source, JSON.stringify(template), { mode: 0o600 });
    await save();
    await run({ root, template, source, output, save });
  } finally {
    await rm(root, { recursive: true });
  }
}
async function absent(path: string) {
  await assert.rejects(access(path), { code: "ENOENT" });
}

for (const backend of ["firecracker", "vz"] as const) {
  test(`${backend}: hashes fresh staged files and preserves distinct producer facts`, async () => {
    await fixture(backend, async ({ template, source, output }) => {
      const inputs = await materializeInputs(backend, source, output);
      assert.deepEqual(parseInputs(JSON.parse(await readFile(output, "utf8")), backend), inputs);
      assert.equal((await lstat(output)).mode & 0o777, 0o600);
      assert.equal(inputs.sourceCommit, template.sourceCommit);
      const pins = [
        ...Object.values(inputs.binaries),
        ...Object.values(inputs.images).map((i) => i.archive),
        inputs.proxy,
        inputs.firecracker ?? inputs.nativeProbe!,
      ];
      for (const pin of pins)
        assert.equal(
          pin.sha256,
          createHash("sha256")
            .update(await readFile(pin.path))
            .digest("hex"),
        );
      for (const image of Object.values(inputs.images)) {
        assert.equal(image.id, `sha256:${"c".repeat(64)}`);
        assert.equal(image.manifestDigest, `sha256:${"d".repeat(64)}`);
        assert.notEqual(`sha256:${image.archive.sha256}`, image.id);
        assert.notEqual(image.id, image.manifestDigest);
      }
    });
  });
}

const invalidTemplates: Record<string, (t: Template) => void> = {
  "extra top-level field": (t) => Object.assign(t, { approved: true }),
  "missing top-level field": (t) => {
    Reflect.deleteProperty(t, "sourceCommit");
  },
  "missing binary": (t) => {
    Reflect.deleteProperty(t.binaries, "node");
  },
  "extra binary": (t) => Object.assign(t.binaries, { unknown: t.proxy }),
  "missing image": (t) => {
    Reflect.deleteProperty(t.images, "guest");
  },
  "extra image": (t) => {
    t.images.unknown = t.images.guest!;
  },
  "extra image fact": (t) => Object.assign(t.images.guest!, { passed: true }),
  "missing manifest digest": (t) => {
    Reflect.deleteProperty(t.images.guest!, "manifestDigest");
  },
  "invalid config digest": (t) => {
    t.images.guest!.id = "latest";
  },
  "backend mismatch": (t) => {
    t.backend = "firecracker";
  },
  "wrong architecture": (t) => {
    t.architecture = "amd64";
  },
  "missing conditional pin": (t) => {
    delete t.nativeProbe;
  },
  "wrong conditional pin": (t) => {
    t.firecracker = t.proxy;
  },
  "wrong conditional image": (t) => {
    t.images.hostAgent = t.images.guest!;
  },
  "binary hash supplied": (t) => Object.assign(t.binaries.node, { sha256: "a".repeat(64) }),
  "proxy hash supplied": (t) => Object.assign(t.proxy, { sha256: "a".repeat(64) }),
  "probe hash supplied": (t) => Object.assign(t.nativeProbe!, { sha256: "a".repeat(64) }),
  "archive hash supplied": (t) =>
    Object.assign(t.images.guest!.archive, { sha256: "a".repeat(64) }),
};
for (const [name, mutate] of Object.entries(invalidTemplates))
  test(`rejects ${name} without output`, async () => {
    await fixture("vz", async ({ template, source, output, save }) => {
      mutate(template);
      await save();
      await assert.rejects(materializeInputs("vz", source, output));
      await absent(output);
    });
  });

test("FC rejects a missing host image, a native probe, and a supplied firecracker hash", async () => {
  for (const mutate of [
    (t: Template) => {
      delete t.images.hostAgent;
    },
    (t: Template) => {
      t.nativeProbe = t.proxy;
    },
    (t: Template) => Object.assign(t.firecracker!, { sha256: "a".repeat(64) }),
  ])
    await fixture("firecracker", async ({ template, source, output, save }) => {
      mutate(template);
      await save();
      await assert.rejects(materializeInputs("firecracker", source, output));
      await absent(output);
    });
});

for (const kind of [
  "missing",
  "empty",
  "symlink",
  "hardlink",
  "directory",
  "noncanonical",
  "symlink-parent",
] as const)
  test(`rejects ${kind} artifact without output`, async () => {
    await fixture("vz", async ({ root, template, source, output, save }) => {
      const path = join(root, "bad");
      if (kind === "empty") await writeFile(path, "");
      if (kind === "symlink") await symlink(template.proxy.path, path);
      if (kind === "hardlink") await link(template.proxy.path, path);
      if (kind === "directory") await mkdir(path);
      if (kind === "symlink-parent") await symlink(root, path);
      template.binaries.node.path =
        kind === "noncanonical"
          ? `${root}/../${root.split("/").at(-1)}/proxy`
          : kind === "symlink-parent"
            ? join(path, "proxy")
            : path;
      await save();
      await assert.rejects(materializeInputs("vz", source, output));
      await absent(output);
    });
  });

test(
  "rejects an unowned artifact when file-owner changes are available",
  { skip: process.getuid?.() !== 0 },
  async () => {
    await fixture("vz", async ({ template, source, output }) => {
      await chown(template.binaries.node.path, 1, 1);
      await assert.rejects(materializeInputs("vz", source, output));
      await absent(output);
    });
  },
);

for (const kind of ["public", "oversize", "symlink", "hardlink"] as const)
  test(`rejects ${kind} template without output`, async () => {
    await fixture("vz", async ({ root, source, output }) => {
      if (kind === "public") await chmod(source, 0o644);
      if (kind === "oversize") await writeFile(source, " ".repeat((1 << 20) + 1));
      let input = source;
      if (kind === "symlink" || kind === "hardlink") {
        input = join(root, "linked-template");
        await (kind === "symlink" ? symlink : link)(source, input);
      }
      await assert.rejects(materializeInputs("vz", input, output));
      await absent(output);
    });
  });

for (const kind of ["existing", "symlink", "hardlink", "public-parent", "noncanonical"] as const)
  test(`rejects ${kind} output`, async () => {
    await fixture("vz", async ({ root, source, output }) => {
      if (kind === "existing") await writeFile(output, "retain", { mode: 0o600 });
      if (kind === "symlink") await symlink(source, output);
      if (kind === "hardlink") await link(source, output);
      if (kind === "public-parent") await chmod(root, 0o755);
      const target = kind === "noncanonical" ? `${root}/./inputs.json` : output;
      await assert.rejects(materializeInputs("vz", source, target));
      if (kind === "existing") assert.equal(await readFile(output, "utf8"), "retain");
      if (kind === "public-parent" || kind === "noncanonical") await absent(output);
    });
  });

test("CLI accepts exact flags, rejects unknown backend and extra arguments, and runs with a cleared environment", async () => {
  assert.deepEqual(
    cli(["--backend", "vz", "--template", "/private/template", "--output", "/private/output"]),
    { backend: "vz", template: "/private/template", output: "/private/output" },
  );
  for (const args of [
    [],
    ["--backend", "process", "--template", "/t", "--output", "/o"],
    ["--backend", "vz", "--template", "relative", "--output", "/o"],
    ["--backend", "vz", "--output", "/o", "--template", "/t"],
    ["--backend", "vz", "--template", "/t", "--output", "/o", "--extra"],
  ])
    assert.throws(() => cli(args));
  await fixture("vz", async ({ root, source, output }) => {
    const result = spawnSync(
      process.execPath,
      [
        "--experimental-transform-types",
        join(REPO, ".github/scripts/mcp-stack-inputs.mts"),
        "--backend",
        "vz",
        "--template",
        source,
        "--output",
        output,
      ],
      {
        cwd: REPO,
        env: { PATH: dirname(process.execPath), TMPDIR: root },
        encoding: "utf8",
        timeout: 10000,
      },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "");
    parseInputs(JSON.parse(await readFile(output, "utf8")), "vz");
  });
});

test("CLI writes Inputs from a source snapshot before dependencies are installed", async () => {
  await fixture("vz", async ({ root, template, source, output }) => {
    const snapshot = join(root, "source");
    for (const path of [
      ".github/scripts/mcp-stack-inputs.mts",
      "tests/fixtures/mcp-stack/platform",
      "orchestrator/scripts/mcp-stack",
    ]) {
      const target = join(snapshot, path);
      await mkdir(dirname(target), { recursive: true });
      await cp(join(REPO, path), target, { recursive: true });
    }
    await absent(join(snapshot, "node_modules"));
    await absent(join(snapshot, "orchestrator/node_modules"));
    const result = spawnSync(
      process.execPath,
      [
        "--experimental-transform-types",
        join(snapshot, ".github/scripts/mcp-stack-inputs.mts"),
        "--backend",
        "vz",
        "--template",
        source,
        "--output",
        output,
      ],
      {
        cwd: snapshot,
        env: { PATH: dirname(process.execPath), TMPDIR: root },
        encoding: "utf8",
        timeout: 10000,
      },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "");
    const inputs = parseInputs(JSON.parse(await readFile(output, "utf8")), "vz");
    assert.equal((await lstat(output)).mode & 0o777, 0o600);
    assert.equal(inputs.sourceCommit, template.sourceCommit);
    assert.equal(
      inputs.binaries.node.sha256,
      createHash("sha256").update(await readFile(template.binaries.node.path)).digest("hex"),
    );
    assert.equal(inputs.images.guest.id, template.images.guest!.id);
    assert.equal(inputs.images.guest.manifestDigest, template.images.guest!.manifestDigest);
  });
});
