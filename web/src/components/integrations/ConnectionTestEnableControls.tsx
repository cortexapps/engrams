/**
 * ConnectionTestEnableControls — the one test/enable control pair for a named
 * integration connection (ADR 0109). The connections list and the setup page
 * previously duplicated these buttons with divergent gating; this component
 * owns the mutations and the gate, and reports outcomes to the caller for
 * presentation (row text, banner, toast).
 *
 * Gate: Disable is always available; Enable requires a passed test — the
 * server-stamped `testedAt`, or a test that passed in this mount before the
 * refreshed connection row lands.
 */

import { useState } from "react";
import { Loader2Icon, PlayIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { useSetConnectionEnabled, useTestConnection } from "@/hooks/useIntegrations";
import { errorMessage } from "@/lib/errors";
import type { IntegrationConnection } from "@/gen/engram/app/v1/integration_pb";

export type ConnectionControlResult =
  | { kind: "test"; ok: boolean; message: string }
  | { kind: "enable"; ok: boolean; enabled: boolean; message: string };

export function ConnectionTestEnableControls({
  connection,
  compact = false,
  testDisabled = false,
  onResult,
}: {
  connection: IntegrationConnection;
  /** Compact row form: small buttons, short labels, no icons. */
  compact?: boolean;
  /** Extra gate for Test (the setup page waits for the generated setup). */
  testDisabled?: boolean;
  onResult: (result: ConnectionControlResult) => void;
}) {
  const test = useTestConnection();
  const enable = useSetConnectionEnabled();
  // A test that passes here unlocks Enable immediately; the invalidated
  // connections query then delivers the server-stamped `testedAt`.
  const [testPassed, setTestPassed] = useState(false);

  const runTest = async () => {
    try {
      const response = await test.mutateAsync({ id: connection.id });
      if (response.ok) setTestPassed(true);
      onResult({ kind: "test", ok: response.ok, message: response.message });
    } catch (error) {
      onResult({ kind: "test", ok: false, message: errorMessage(error) });
    }
  };

  const setEnabled = async () => {
    const next = !connection.enabled;
    try {
      await enable.mutateAsync({ id: connection.id, enabled: next });
      onResult({ kind: "enable", ok: true, enabled: next, message: "" });
    } catch (error) {
      onResult({
        kind: "enable",
        ok: false,
        enabled: connection.enabled,
        message: errorMessage(error),
      });
    }
  };

  const size = compact ? "sm" : "default";
  const canEnable = connection.enabled || Boolean(connection.testedAt) || testPassed;

  return (
    <>
      <Button
        variant="outline"
        size={size}
        disabled={test.isPending || testDisabled}
        onClick={() => void runTest()}
      >
        {!compact &&
          (test.isPending ? (
            <Loader2Icon className="size-4 animate-spin" />
          ) : (
            <PlayIcon className="size-4" />
          ))}
        {compact ? "Test" : "Test connection"}
      </Button>
      <Button
        size={size}
        variant={compact && connection.enabled ? "outline" : "default"}
        disabled={enable.isPending || !canEnable}
        onClick={() => void setEnabled()}
      >
        {!compact && enable.isPending && <Loader2Icon className="size-4 animate-spin" />}
        {connection.enabled
          ? compact
            ? "Disable"
            : "Disable connection"
          : compact
            ? "Enable"
            : "Enable connection"}
      </Button>
    </>
  );
}
