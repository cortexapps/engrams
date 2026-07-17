import { isUniqueViolation } from "../db/pg-errors.ts";
import type { ProfileStore } from "../db/profiles.ts";
import { DEFAULT_PROFILE_NETWORK } from "../db/schema.ts";
import { PR_REVIEW_CAPABILITY } from "../tools/review.ts";

export const PR_REVIEWER_DESIGNATION = "pr_reviewer";

export interface ReviewerProfileSeedLogger {
  info(message: string): void;
  debug(bindings: Record<string, unknown>, message: string): void;
  error(bindings: Record<string, unknown>, message: string): void;
}

/** Best-effort bootstrap of the system profile used by the PR review workflow. */
export async function seedReviewerProfile(
  store: ProfileStore,
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
        skills: [],
        capabilities: [PR_REVIEW_CAPABILITY],
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
