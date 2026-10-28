import { ConnectError } from "@connectrpc/connect";

/** A thrown error as a person reads it in a step's ledger row. A
 * ConnectError's `message` carries its code in brackets
 * (`[internal] policy secret X is not configured`), which names a transport
 * class, not a cause; the row shows the text. */
export function errorMessage(error: unknown): string {
  if (error instanceof ConnectError) return error.rawMessage;
  return error instanceof Error ? error.message : String(error);
}
