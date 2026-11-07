import {
  bindingTool,
  createCatalog,
  createFixtureProfile,
  createHumanTask,
  waitFor,
} from "./bootstrap.ts";
import { requireThat, sha256, type Context } from "./context.ts";
import type { InfrastructureRow } from "./evidence.ts";
import {
  canonicalJson,
  guestExec,
  guestReady,
  readGuestFile,
} from "./guest.ts";
import { DatabaseObserver, FixtureObserver } from "./observer.ts";
import type { Principals } from "./scenario-runtime.ts";
import { anonymousGuestFileStatus } from "./transport.ts";

export type InfrastructureEvidence = InfrastructureRow[];

/** Exercise the public owner file routes before any model or MCP tool runs. */
export async function runInfrastructure(
  ctx: Context,
  principals: Principals,
  database: DatabaseObserver,
): Promise<InfrastructureEvidence> {
  const fixture = new FixtureObserver(ctx, "modern");
  const before = await fixture.observe();
  const rows: InfrastructureEvidence = [];
  for (const mode of ["ordinary", "managed"] as const) {
    let profileId: string | undefined;
    let taskId: string | undefined;
    let failed = false;
    // Retain ownership as soon as Create returns, including later helper failures.
    const admin = {
      ...principals.admin,
      profiles: {
        ...principals.admin.profiles,
        createProfile: async (
          ...args: Parameters<typeof principals.admin.profiles.createProfile>
        ) => {
          const response = await principals.admin.profiles.createProfile(
            ...args,
          );
          profileId = response.profile?.id;
          return response;
        },
      },
    };
    const owner = {
      ...principals.owner,
      tasks: {
        ...principals.owner.tasks,
        createTask: async (
          ...args: Parameters<typeof principals.owner.tasks.createTask>
        ) => {
          const response = await principals.owner.tasks.createTask(...args);
          taskId = response.task?.id;
          return response;
        },
      },
    };
    try {
      let managed: Awaited<ReturnType<typeof createCatalog>> | undefined;
      let alias: string | undefined;
      if (mode === "ordinary") {
        const response = await admin.profiles.createProfile({
          name: "Fixture infrastructure ordinary",
          imageId: ctx.imageId,
          harness: "claude",
          modelRouter: "openrouter",
          model: "z-ai/glm-5.2",
          integrationGrants: [],
        });
        requireThat(response.profile?.id, "infrastructure_ordinary_profile");
      } else {
        managed = await createCatalog(
          ctx,
          admin,
          "infrastructure-managed",
          "modern",
          "api_key",
        );
        const profile = await createFixtureProfile(ctx, admin, {
          harness: "claude",
          suffix: "infrastructure-managed",
          connectionId: managed.connection.id,
          connectionAlias: managed.connection.alias,
          upstreamName: "read_counter",
          mode: "read",
          grant: true,
          automationWrite: false,
          arguments: {},
          callId: "infrastructure-read",
          constraints: [],
          defaultAllow: false,
        });
        alias = profile.alias;
      }
      requireThat(profileId, "infrastructure_profile");
      const task = await createHumanTask(ctx, owner, profileId, "claude");
      database.own(task.taskId, task.sessionId);
      if (managed) {
        const coordinator = await waitFor(
          ctx,
          "infrastructure_coordinator_started",
          () => database.started(task.sessionId, ctx.hostId),
          (state) => state.phase === "started",
        );
        requireThat(
          coordinator.source_key.context.operator_identity ===
            ctx.publication.candidateIdentity &&
            coordinator.source_key.context.target.session_id ===
              task.sessionId &&
            coordinator.source_key.context.target.host_id === ctx.hostId &&
            coordinator.source_key.creation.task_id === task.taskId &&
            coordinator.source_key.creation.session_id === task.sessionId &&
            coordinator.source_key.creation.policy.actorId ===
              principals.actorIds.owner,
          "infrastructure_started_owner_publication",
        );
        await waitFor(
          ctx,
          "infrastructure_orchestrator_started",
          () => database.managedLifecycle(task.sessionId, coordinator),
          (state) => state.phase === "started",
        );
        const bound = await bindingTool(
          owner,
          task.sessionId,
          managed.connection.id,
          "read_counter",
        );
        const reviewed = managed.catalog.tools.find(
          (tool) => tool.upstreamName === "read_counter",
        );
        requireThat(
          bound.modelToolName === alias &&
            bound.reviewedTool &&
            reviewed &&
            canonicalJson(JSON.parse(bound.reviewedTool.inputSchemaJson)) ===
              canonicalJson(JSON.parse(reviewed.inputSchemaJson)),
          "infrastructure_delivered_read_binding",
        );
      }
      await guestReady(ctx, owner, task.sessionId);
      const bytes = Buffer.alloc(4096, 65);
      const uploadedPath = `${ctx.guest.stateDirectory}/infrastructure-${mode}-upload.bin`;
      const createdPath = `${ctx.guest.stateDirectory}/infrastructure-${mode}-exec.bin`;
      const upload = await owner.uploadGuestFile(
        task.sessionId,
        uploadedPath,
        bytes,
      );
      const exec = await guestExec(ctx, owner, task.sessionId, [
        "/bin/sh",
        "-c",
        `set -eu
root="$1"
upload="$2"
created="$3"
expected="$4"
uid=$(id -u)
test -d "$root" && test ! -L "$root"
test "$(stat -c %a "$root")" = 700
test "$(stat -c %u "$root")" = "$uid"
check_file() {
  test -f "$1" && test ! -L "$1"
  test "$(stat -c %h "$1")" = 1
  test "$(stat -c %u "$1")" = "$uid"
  test "$(stat -c %a "$1")" = 600
  test "$(stat -c %s "$1")" = 4096
  test "$(sha256sum "$1" | cut -d ' ' -f 1)" = "$expected"
}
check_file "$upload"
umask 077
set -C
head -c 4096 /dev/zero | tr '\\000' A > "$created"
check_file "$created"
printf 'infrastructure-exec-ok\\n'
`,
        "fixture-infrastructure",
        ctx.guest.stateDirectory,
        uploadedPath,
        createdPath,
        sha256(bytes),
      ]);
      requireThat(
        exec.status === 0 &&
          exec.stdout.equals(Buffer.from("infrastructure-exec-ok\n")),
        "infrastructure_exec_integrity",
      );
      const download = await readGuestFile(
        ctx,
        owner,
        task.sessionId,
        createdPath,
      );
      requireThat(
        download.equals(bytes) &&
          upload.sizeBytes === bytes.length &&
          upload.sha256 === sha256(download),
        "infrastructure_transfer_integrity",
      );
      const unauthenticatedStatus = await anonymousGuestFileStatus(
        ctx,
        task.sessionId,
        createdPath,
      );
      const nonOwnerStatus = await principals.other.nonOwnerGuestFileStatus(
        task.sessionId,
        createdPath,
      );
      const after = await fixture.observe();
      requireThat(
        after.reads === before.reads &&
          after.held === before.held &&
          after.cancellations === before.cancellations &&
          canonicalJson(after.effects) === canonicalJson(before.effects),
        "infrastructure_no_tool_execution",
      );
      rows.push({
        mode,
        execSha256: sha256(exec.stdout),
        uploadSha256: upload.sha256,
        downloadSha256: sha256(download),
        sizeBytes: download.length,
        unauthenticatedStatus,
        nonOwnerStatus,
      });
    } catch (error) {
      failed = true;
      throw error;
    } finally {
      let cleanupError: unknown;
      try {
        if (taskId) await principals.admin.tasks.deleteTask({ taskId });
      } catch (error) {
        cleanupError = error;
      }
      try {
        if (profileId)
          await principals.admin.profiles.deleteProfile({ id: profileId });
      } catch (error) {
        cleanupError ??= error;
      }
      // Preserve the first failed boundary; the platform still audits all owned resources.
      if (!failed && cleanupError) throw cleanupError;
    }
  }
  return rows;
}
