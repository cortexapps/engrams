import { expect, spyOn, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import { ExecOutputSchema } from "../../src/gen/engram/app/v1/session_pb.ts";
import type { Context } from "./context.ts";
import { guestReady } from "./guest.ts";
import { Supervisor } from "./supervisor.ts";

for (const backend of ["firecracker", "vz"] as const)
  test(`guest readiness waits for the native read proof only on FC (${backend})`, async () => {
    const order: string[] = [];
    const commands: string[][] = [];
    const prepare = spyOn(
      Supervisor.prototype,
      "prepareDiskRead",
    ).mockImplementation(async () => {
      order.push("prepare");
    });
    const verify = spyOn(
      Supervisor.prototype,
      "verifyDiskRead",
    ).mockImplementation(async () => {
      order.push("verify");
    });
    // Only backend is read while the native control methods are replaced.
    const ctx = { backend, deadlines: { requestMs: 1000 } } as Context;
    const client = {
      sessions: {
        async *exec(request: { argv?: string[] }) {
          const argv = request.argv ?? [];
          commands.push(argv);
          const reading = argv.includes("fixture-disk-read");
          order.push(
            reading
              ? "read"
              : argv.includes("fixture-disk-read-create")
                ? "create"
                : argv.includes("fixture-disk-read-remove")
                  ? "remove"
                  : "ready",
          );
          yield create(ExecOutputSchema, {
            event: { case: "started", value: {} },
          });
          if (reading)
            yield create(ExecOutputSchema, {
              event: { case: "stdout", value: Buffer.alloc(4096) },
            });
          yield create(ExecOutputSchema, {
            event: { case: "exit", value: { exitStatus: 0 } },
          });
        },
      },
    };
    try {
      await guestReady(ctx, client, "session");
      order.push("caller");
      expect(order).toEqual(
        backend === "firecracker"
          ? ["ready", "create", "prepare", "read", "verify", "remove", "caller"]
          : ["ready", "caller"],
      );
      if (backend === "firecracker") {
        const createCommand = commands[1]!;
        const readCommand = commands[2]!;
        expect(createCommand[2]).toContain("conv=excl,fsync");
        expect(createCommand[2]).toContain('"$(stat -c %d /)"');
        expect(createCommand[2]).toContain('"$(stat -c %b "$1")" -ge 8');
        expect(readCommand[2]).toContain(
          "iflag=direct,nofollow bs=4096 count=1",
        );
        expect(createCommand.at(-1)).toMatch(
          /^\/tmp\/mcp-stack-fixture\/disk-read-[a-f0-9]{32}\.bin$/,
        );
        expect(readCommand.at(-1)).toBe(createCommand.at(-1));
      }
    } finally {
      prepare.mockRestore();
      verify.mockRestore();
    }
  });

for (const failure of ["create", "prepare", "read", "verify"] as const)
  test(`guest readiness does not return before a successful read ACK (${failure})`, async () => {
    const order: string[] = [];
    const prepare = spyOn(
      Supervisor.prototype,
      "prepareDiskRead",
    ).mockImplementation(async () => {
      order.push("prepare");
      if (failure === "prepare") throw Error("prepare failed");
    });
    const verify = spyOn(
      Supervisor.prototype,
      "verifyDiskRead",
    ).mockImplementation(async () => {
      order.push("verify");
      if (failure === "verify") throw Error("verify failed");
    });
    // The stub supplies only the fields consumed before native control I/O.
    const ctx = {
      backend: "firecracker",
      deadlines: { requestMs: 1000 },
    } as Context;
    const client = {
      sessions: {
        async *exec(request: { argv?: string[] }) {
          const argv = request.argv ?? [];
          const stage = argv.includes("fixture-disk-read-create")
            ? "create"
            : argv.includes("fixture-disk-read")
              ? "read"
              : "ready";
          order.push(stage);
          yield create(ExecOutputSchema, {
            event: { case: "started", value: {} },
          });
          if (stage === "read")
            yield create(ExecOutputSchema, {
              event: { case: "stdout", value: Buffer.alloc(4096) },
            });
          yield create(ExecOutputSchema, {
            event: {
              case: "exit",
              value: { exitStatus: stage === failure ? 1 : 0 },
            },
          });
        },
      },
    };
    try {
      await expect(guestReady(ctx, client, "session")).rejects.toThrow();
      const steps = ["ready", "create", "prepare", "read", "verify"];
      expect(order).toEqual(steps.slice(0, steps.indexOf(failure) + 1));
    } finally {
      prepare.mockRestore();
      verify.mockRestore();
    }
  });
