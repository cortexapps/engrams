import type { CredentialAdmin } from "./credential-admin.ts";
import type { CredentialMetadataStore } from "./credential-store.ts";
import { validateMcpConfig } from "./config.ts";
import type { makeMcpOperatorRuntime } from "./operator-runtime.ts";

/** New credential authority requires a published destination. Recovery and revocation
 * remain available while publication or new dispatch is disabled. */
export function makePublishedCredentialAdmin(deps: {
  admin: CredentialAdmin;
  store: Pick<CredentialMetadataStore, "get">;
  operator: ReturnType<typeof makeMcpOperatorRuntime>;
}): CredentialAdmin {
  async function current(id: string) {
    const row = await deps.store.get(id);
    await deps.operator.validateDestination(validateMcpConfig(row.config));
  }
  return {
    ...deps.admin,
    async putSecret(...args) {
      await current(args[0]);
      return deps.admin.putSecret(...args);
    },
    async begin(...args) {
      await current(args[0]);
      return deps.admin.begin(...args);
    },
    async complete(...args) {
      await current(args[0]);
      return deps.admin.complete(...args);
    },
    async configure(...args) {
      await deps.operator.validateDestination(args[4]);
      return deps.admin.configure(...args);
    },
  };
}
