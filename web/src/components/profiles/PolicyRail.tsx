/**
 * PolicyRail — the live "Session policy" receipts shown beside the profile
 * editor: image, powers (per provider + counts + write marker), reachable hosts
 * (or "Fully sandboxed"), credentials with provenance, and the skills/user-token
 * line. Mirrors what `compileIntegrationPolicy` ships on CreateSession.
 */

import {
  BanIcon,
  KeyRoundIcon,
  LockIcon,
  PencilIcon,
  ShieldCheckIcon,
  SparklesIcon,
  UserIcon,
} from "lucide-react";

import { Card } from "@/components/ui/card";
import { Text } from "@/components/ui/text";
import { ProviderTile } from "@/components/integrations/ProviderTile";
import type { DerivedPolicy } from "@/lib/profilePolicy";

export function PolicyRail({
  policy,
  imageUri,
  skillsCount,
  includeUserTokens,
}: {
  policy: DerivedPolicy;
  imageUri?: string;
  skillsCount: number;
  includeUserTokens: boolean;
}) {
  return (
    <Card className="gap-0 overflow-hidden py-0">
      <div className="flex items-center gap-2 border-b bg-secondary px-4 py-3">
        <ShieldCheckIcon className="size-3.5 text-instrument-nominal" />
        <h2 className="text-sm font-semibold">Session policy</h2>
      </div>
      <div className="flex flex-col gap-4 px-4 py-3.5">
        <p className="text-xs leading-relaxed text-muted-foreground">
          What a session launched from this profile is actually granted.
        </p>

        <Row label="Image">
          <span className="font-mono text-xs break-all">{imageUri || "—"}</span>
        </Row>

        <Row label={`Powers · ${policy.capCount}`}>
          {policy.providers.length === 0 ? (
            <Muted>None — read-nothing, write-nothing.</Muted>
          ) : (
            <div className="flex flex-col gap-1.5">
              {policy.providers.map((p) => (
                <div key={p.view.provider} className="flex items-center gap-2">
                  <ProviderTile {...p.view.icon} name={p.view.name} size={18} />
                  <span className="flex-1 text-xs">{p.view.name}</span>
                  <span className="font-mono text-xs text-muted-foreground">{p.caps.length}</span>
                  {p.caps.some((c) => c.access === "write") && (
                    <PencilIcon className="size-3 text-instrument-caution" />
                  )}
                </div>
              ))}
            </div>
          )}
        </Row>

        <Row label={`Can reach · ${policy.reachable.length}`}>
          {policy.reachable.length === 0 ? (
            <span className="inline-flex items-center gap-1.5 text-xs text-instrument-nominal">
              <LockIcon className="size-3.5" />
              Fully sandboxed
            </span>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {policy.reachable.map((h) => (
                <span
                  key={h}
                  className="rounded-sm border bg-secondary px-2 py-px font-mono text-2xs"
                >
                  {h}
                </span>
              ))}
            </div>
          )}
        </Row>

        <Row label={`Credentials · ${policy.credentials.length}`}>
          {policy.credentials.length === 0 ? (
            <Muted>None.</Muted>
          ) : (
            <div className="flex flex-col gap-1.5">
              {policy.credentials.map((cr, i) => {
                const Icon =
                  cr.kind === "mint"
                    ? ShieldCheckIcon
                    : cr.kind === "literal"
                      ? KeyRoundIcon
                      : LockIcon;
                return (
                  <div key={i} className="flex items-start gap-2">
                    <Icon
                      className={`mt-0.5 size-3.5 shrink-0 ${cr.kind === "literal" ? "text-instrument-caution" : "text-instrument-nominal"}`}
                    />
                    <div className="min-w-0">
                      <div className="text-xs">{cr.label}</div>
                      <div className="text-2xs text-muted-foreground">{cr.detail}</div>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </Row>

        <div className="flex gap-3.5 pt-0.5">
          <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
            <SparklesIcon className="size-3.5" />
            {skillsCount} skill{skillsCount === 1 ? "" : "s"}
          </span>
          <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
            {includeUserTokens ? (
              <UserIcon className="size-3.5" />
            ) : (
              <BanIcon className="size-3.5" />
            )}
            {includeUserTokens ? "carries other tokens" : "harness token only"}
          </span>
        </div>
      </div>
    </Card>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1.5">
      <Text variant="label" className="text-2xs">
        {label}
      </Text>
      {children}
    </div>
  );
}

function Muted({ children }: { children: React.ReactNode }) {
  return <span className="text-xs text-muted-foreground">{children}</span>;
}
