import { interactiveApprovalDriver } from "./case-approval.ts";
import { killCancelDriver } from "./case-controls.ts";
import { catalogFidelityDriver } from "./case-fidelity.ts";
import {
  grantDenialsDriver,
  unattendedGrantDriver,
  planChildIntersectionDriver,
} from "./case-grants.ts";
import { oauthAuthorityDriver } from "./case-oauth.ts";
import {
  credentialNetworkIsolationDriver,
  diskOnlyColdRebindDriver,
} from "./case-physical.ts";
import {
  heldWriteRestartDriver,
  outboxGuestAckDriver,
  revokeResumeDriver,
} from "./case-recovery.ts";
import { main, type DriverRegistry } from "./runner.ts";

export const drivers: DriverRegistry = {
  "catalog-fidelity": catalogFidelityDriver,
  "grant-denials": grantDenialsDriver,
  "interactive-approval": interactiveApprovalDriver,
  "unattended-grant": unattendedGrantDriver,
  "plan-child-intersection": planChildIntersectionDriver,
  "credential-network-isolation": credentialNetworkIsolationDriver,
  "disk-only-cold-rebind": diskOnlyColdRebindDriver,
  "revoke-resume": revokeResumeDriver,
  "held-write-restart": heldWriteRestartDriver,
  "oauth-authority": oauthAuthorityDriver,
  "kill-cancel": killCancelDriver,
  "outbox-guest-ack": outboxGuestAckDriver,
};
if (import.meta.main) await main(drivers, process.argv.slice(2));
