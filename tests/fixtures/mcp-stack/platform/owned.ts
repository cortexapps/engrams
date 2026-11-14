import { createHash, timingSafeEqual } from "node:crypto";
import { constants } from "node:fs";
import { lstat, mkdir, open, realpath, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, join, resolve, sep } from "node:path";
import { spawn, type ChildProcess } from "node:child_process";

export class PlatformFailure extends Error {
  constructor(
    readonly code: "NEEDS_CONTEXT" | "INVALID_CONTEXT" | "FAILED",
    readonly seam: string,
  ) {
    super(`${code}:${seam}`);
  }
}
export function check(value: unknown, seam: string): asserts value {
  if (!value) throw new PlatformFailure("INVALID_CONTEXT", seam);
}
export const hash = (bytes: string | Uint8Array) =>
  createHash("sha256").update(bytes).digest("hex");
export function sameSecret(a: string, b: string): boolean {
  const aa = Buffer.from(a),
    bb = Buffer.from(b);
  return aa.length === bb.length && timingSafeEqual(aa, bb);
}
export async function ownedPath(path: string, absent = false): Promise<void> {
  check(isAbsolute(path) && resolve(path) === path, "absolute_path");
  let current: string = sep;
  for (const component of path.split(sep).filter(Boolean)) {
    current = join(current, component);
    try {
      const s = await lstat(current);
      check(
        !s.isSymbolicLink() && (current === path || s.isDirectory()),
        "real_path",
      );
      if (current === path) check(s.uid === process.getuid?.(), "path_owner");
    } catch (e) {
      if (
        absent &&
        current === path &&
        e instanceof Error &&
        "code" in e &&
        e.code === "ENOENT"
      )
        return;
      throw e;
    }
  }
  if (absent) throw new PlatformFailure("INVALID_CONTEXT", "output_exists");
}
export async function privateRoot(path: string): Promise<void> {
  await ownedPath(path);
  const s = await lstat(path);
  check(
    s.isDirectory() &&
      (s.mode & 0o777) === 0o700 &&
      (await realpath(path)) === path,
    "private_root",
  );
}
export async function newRoot(path: string): Promise<void> {
  await privateRoot(dirname(path));
  await ownedPath(path, true);
  await mkdir(path, { mode: 0o700 });
  await privateRoot(path);
}
export async function readOwned(
  path: string,
  limit = 1 << 20,
  privateOnly = true,
): Promise<Buffer> {
  await ownedPath(path);
  const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const s = await handle.stat();
    check(
      s.isFile() &&
        s.uid === process.getuid?.() &&
        s.nlink === 1 &&
        s.size > 0 &&
        s.size <= limit,
      "file_identity",
    );
    check(!privateOnly || (s.mode & 0o077) === 0, "private_file");
    const bytes = await handle.readFile();
    check(bytes.length === s.size, "file_changed");
    const after = await handle.stat();
    check(
      after.mtimeMs === s.mtimeMs &&
        after.ctimeMs === s.ctimeMs &&
        after.nlink === 1,
      "file_changed",
    );
    return bytes;
  } finally {
    await handle.close();
  }
}
export interface Pin {
  path: string;
  sha256: string;
}
export async function verifyPin(pin: Pin): Promise<void> {
  check(/^[a-f0-9]{64}$/.test(pin.sha256), "pin_shape");
  await ownedPath(pin.path);
  const h = await open(pin.path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const s = await h.stat();
    check(
      s.isFile() && s.nlink === 1 && s.uid === process.getuid?.() && s.size > 0,
      "artifact_identity",
    );
    const digest = createHash("sha256");
    for await (const bytes of h.createReadStream({ autoClose: false }))
      digest.update(bytes);
    const after = await h.stat();
    check(
      after.mtimeMs === s.mtimeMs &&
        after.ctimeMs === s.ctimeMs &&
        after.nlink === 1 &&
        digest.digest("hex") === pin.sha256,
      "artifact_hash",
    );
  } finally {
    await h.close();
  }
}
export async function put(
  path: string,
  value: string | Uint8Array,
  executable = false,
): Promise<Pin> {
  await privateRoot(dirname(path));
  await ownedPath(path, true);
  await writeFile(path, value, {
    mode: executable ? 0o700 : 0o600,
    flag: "wx",
  });
  return { path, sha256: hash(value) };
}
export function record(
  value: unknown,
  keys: readonly string[],
  optional: readonly string[] = [],
): Record<string, unknown> {
  check(
    value !== null && typeof value === "object" && !Array.isArray(value),
    "object",
  );
  const data = value as Record<string, unknown>;
  check(
    keys.every((key) => Object.hasOwn(data, key)) &&
      Object.keys(data).every(
        (key) => keys.includes(key) || optional.includes(key),
      ),
    "object_keys",
  );
  return data;
}

// Only explicit task values cross the child boundary. No shell, HOME, cloud or agent environment is inherited.
export function childEnvironment(
  tmp: string,
  values: Readonly<Record<string, string>> = {},
): NodeJS.ProcessEnv {
  check(isAbsolute(tmp), "child_tmp");
  check(
    Object.keys(values).every((key) => key === "DOCKER_HOST"),
    "child_environment",
  );
  if (values.DOCKER_HOST !== undefined) {
    const path = values.DOCKER_HOST.slice("unix://".length);
    check(
      values.DOCKER_HOST.startsWith("unix://") &&
        isAbsolute(path) &&
        resolve(path) === path,
      "engine_endpoint",
    );
  }
  return {
    PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
    TMPDIR: tmp,
    LANG: "C",
    ...values,
  };
}
export class OwnedProcess {
  readonly child: ChildProcess;
  readonly done: Promise<void>;
  private ended = false;
  constructor(
    file: string,
    args: readonly string[],
    cwd: string,
    env: NodeJS.ProcessEnv,
    stdio:
      | "ignore"
      | ["ignore", number, number]
      | ["pipe", "pipe", "ignore"]
      | ["pipe", "pipe", "pipe"] = "ignore",
  ) {
    check(isAbsolute(file) && isAbsolute(cwd), "process_path");
    this.child = spawn(file, [...args], { cwd, env, detached: true, stdio });
    this.done = new Promise((resolve) => {
      this.child.once("error", () => {
        this.ended = true;
        resolve();
      });
      this.child.once("close", () => {
        this.ended = true;
        resolve();
      });
    });
  }
  running(): boolean {
    return (
      !!this.child.pid &&
      !this.ended &&
      this.child.exitCode === null &&
      this.child.signalCode === null
    );
  }
  private groupExists(): boolean {
    if (!this.child.pid) return false;
    try {
      process.kill(-this.child.pid, 0);
      return true;
    } catch (e) {
      if (e instanceof Error && "code" in e) {
        if (e.code === "ESRCH") return false;
        // macOS returns EPERM for a group with only unreaped zombies.
        // Permission denial leaves group absence unproved on every platform.
        if (e.code === "EPERM") return true;
      }
      throw e;
    }
  }
  async stop(graceMs = 1500): Promise<void> {
    check(
      Number.isInteger(graceMs) && graceMs >= 0 && graceMs <= 120000,
      "process_deadline",
    );
    const signal = (s: NodeJS.Signals) => {
      if (!this.child.pid) return;
      try {
        process.kill(-this.child.pid, s);
      } catch (e) {
        if (!(
          e instanceof Error &&
          "code" in e &&
          (e.code === "ESRCH" || e.code === "EPERM")
        ))
          throw e;
      }
    };
    if (this.groupExists()) signal("SIGTERM");
    const deadline = performance.now() + graceMs;
    while (this.groupExists() && performance.now() < deadline)
      await new Promise((r) => setTimeout(r, 20));
    if (this.groupExists()) signal("SIGKILL");
    const end = performance.now() + 1500;
    while ((!this.ended || this.groupExists()) && performance.now() < end)
      await new Promise((r) => setTimeout(r, 20));
    if (!this.ended || this.groupExists())
      throw new PlatformFailure("FAILED", "process_cleanup");
    await this.done;
  }
}
export async function command(
  file: string,
  args: readonly string[],
  cwd: string,
  env: NodeJS.ProcessEnv,
  input?: string,
  deadlineMs = 10000,
): Promise<Buffer> {
  check(isAbsolute(file) && isAbsolute(cwd), "process_path");
  check(
    Number.isInteger(deadlineMs) && deadlineMs > 0 && deadlineMs <= 120000,
    "command_deadline",
  );
  const owned = new OwnedProcess(file, args, cwd, env, [
    "pipe",
    "pipe",
    "ignore",
  ]);
  const { child } = owned;
  // These streams are present because this fixed command path selects pipe stdio.
  const { stdin, stdout } = child;
  check(stdin && stdout, "command_stdio");
  const chunks: Buffer[] = [];
  let bytes = 0,
    interrupted = false,
    unavailable = false;
  let interrupt!: () => void;
  const interruption = new Promise<void>((resolve) => {
    interrupt = resolve;
  });
  const timer = setTimeout(() => {
    interrupted = true;
    interrupt();
  }, deadlineMs);
  stdout.on("data", (chunk: Buffer) => {
    bytes += chunk.length;
    if (bytes > 4 * 1024 * 1024) {
      interrupted = true;
      interrupt();
    } else chunks.push(chunk);
  });
  stdout.on("error", () => {
    interrupted = true;
    interrupt();
  });
  stdin.on("error", () => {});
  child.once("error", () => {
    unavailable = true;
  });
  stdin.end(input);
  try {
    await Promise.race([owned.done, interruption]);
    // The leader closing is not group completion. Reuse the retained group's
    // TERM -> KILL -> wait path before either success or command failure.
    await owned.stop();
    if (unavailable)
      throw new PlatformFailure("NEEDS_CONTEXT", "command_unavailable");
    if (interrupted || child.exitCode !== 0)
      throw new PlatformFailure("FAILED", "command");
    return Buffer.concat(chunks);
  } catch (error) {
    if (error instanceof PlatformFailure && error.seam === "process_cleanup")
      throw new PlatformFailure("FAILED", "command_cleanup");
    throw error;
  } finally {
    clearTimeout(timer);
    stdin.destroy();
    stdout.destroy();
    child.unref();
  }
}
