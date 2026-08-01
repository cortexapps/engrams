import { isUniqueViolation } from "../db/pg-errors.ts";
import type { IntegrationConnectionStore } from "../db/integration-connections.ts";
import type { ProfileStore } from "../db/profiles.ts";
import { DEFAULT_PROFILE_NETWORK } from "../db/schema.ts";
import { PR_REVIEW_CAPABILITY } from "../tools/review.ts";
import { capabilityGrant } from "../integrations/grants.ts";

export const PR_REVIEWER_DESIGNATION = "pr_reviewer";

export interface ReviewerProfileSeedLogger {
  info(message: string): void;
  debug(bindings: Record<string, unknown>, message: string): void;
  error(bindings: Record<string, unknown>, message: string): void;
}

/** Best-effort bootstrap of the system profile used by the PR review workflow. */
export async function seedReviewerProfile(
  store: ProfileStore,
  connections: IntegrationConnectionStore,
  log: ReviewerProfileSeedLogger,
): Promise<void> {
  if (await store.getByDesignation(PR_REVIEWER_DESIGNATION)) return;

  const defaultProfile = await store.getDefault();
  if (!defaultProfile) {
    log.info(
      "reviewer profile not seeded: configure an org default profile first, then it seeds on next boot (or designate one manually)",
    );
    return;
  }
  const engramsConnection = await connections.getDefault("engram");
  if (!engramsConnection) {
    throw new Error("default Engrams integration connection is unavailable");
  }

  try {
    await store.create(
      {
        name: "PR Reviewer",
        description:
          "engrams code reviewer for pull requests (ADR 0100). Repoint the image/model as needed; do not delete.",
        icon: "ScanSearch",
        imageId: defaultProfile.imageId,
        harness: defaultProfile.harness,
        model: defaultProfile.model,
        effort: defaultProfile.effort,
        includeUserTokens: false,
        envVars: {},
        // The "skills" bundle ships the git-askpass helper
        // (provides_askpass); without it the review session's forge broker
        // token has no credential wiring and the workflow's clone fails with
        // "could not read Username for 'https://github.com'".
        skills: ["skills"],
        integrationGrants: [capabilityGrant(PR_REVIEW_CAPABILITY, engramsConnection.id)],
        launchAccess: "organization",
        network: DEFAULT_PROFILE_NETWORK,
        secrets: [],
        isDefault: false,
        portExposures: [],
      },
      PR_REVIEWER_DESIGNATION,
    );
  } catch (error) {
    if (isUniqueViolation(error)) {
      log.debug(
        { designation: PR_REVIEWER_DESIGNATION, err: error },
        "reviewer profile already seeded by another replica",
      );
      return;
    }
    log.error({ err: error }, "reviewer profile seed create failed");
    throw error;
  }
}
