/** The shipped built-in definitions keep their content hash.
 *
 * A changed hash makes the boot seed insert a new version and stop every
 * active run of the old one. Change a hash here only when that is intended.
 */

import { expect, test } from "bun:test";

import { PR_REVIEW_DEFINITION } from "../pr-review.ts";
import { definitionContentHash } from "../seed.ts";
import { SLACK_BRAIN_DEFINITION } from "../slack-brain.ts";

test("the PR review definition hash is unchanged", () => {
  expect(definitionContentHash(PR_REVIEW_DEFINITION)).toBe("c11862e4e9faedf8d6727d752f55ad5816f8aca449f4ba2592ec945725b66313");
});

test("the Slack threads definition hash is unchanged", () => {
  expect(definitionContentHash(SLACK_BRAIN_DEFINITION)).toBe("756c76430aec0afecefddf12051c68a27f71b3afa14e6e5674c13e91f4f3c54f");
});
