/**
 * GithubSetupPanel — the "set up the GitHub App" guide shown at the top of the
 * GitHub connect flow, before the App ID / private-key fields. The raw mint
 * fields don't tell an admin how to *create* an App that works: which
 * permissions to grant, which webhook events review-driven sessions need, or
 * where GitHub should deliver them. This panel spells that out and pre-fills the
 * error-prone parts (a copy-paste App manifest + the webhook payload URL),
 * mirroring what OAuthConnectSheet does for Slack.
 *
 * Permissions are split into the always-required minimum and the extra powers
 * this connector can grant — "add more to give sessions more powers." The list
 * is derived from the connector's own capabilities, so it never drifts from what
 * the coordinator will actually mint (see githubSetup.ts).
 */

import { useState } from "react";
import {
  ArrowUpRightIcon,
  CheckIcon,
  CopyIcon,
  KeyRoundIcon,
  ShieldCheckIcon,
  WebhookIcon,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import {
  WEBHOOK_EVENTS,
  buildGithubManifest,
  githubWebhookUrl,
  isMinimumPermission,
  permissionLabel,
  permissionsForCapabilities,
} from "@/lib/githubSetup";
import { AccessTag } from "./chips";
import type { ConnectorView } from "./useConnectorViews";

const NEW_APP_URL = "https://github.com/settings/apps/new";

export function GithubSetupPanel({ view }: { view: ConnectorView }) {
  const [copied, setCopied] = useState<"url" | "manifest" | null>(null);

  const origin = typeof window !== "undefined" ? window.location.origin : "";
  const permissions = permissionsForCapabilities(view.capabilities);
  const webhookUrl = githubWebhookUrl(origin);
  const manifest = buildGithubManifest({ origin, permissions, events: WEBHOOK_EVENTS });

  const copy = (what: "url" | "manifest", text: string) => {
    if (typeof navigator === "undefined" || !navigator.clipboard) return;
    void navigator.clipboard.writeText(text).then(() => {
      setCopied(what);
      setTimeout(() => setCopied((c) => (c === what ? null : c)), 2000);
    });
  };

  const minimum = permissions.filter((p) => isMinimumPermission(p.key));
  const extra = permissions.filter((p) => !isMinimumPermission(p.key));

  return (
    <div className="flex flex-col gap-4 rounded-lg border bg-secondary/40 p-4">
      <div className="flex gap-3">
        <ShieldCheckIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
        <div>
          <div className="text-sm font-semibold">Set up the GitHub App first</div>
          <div className="mt-0.5 text-[0.78rem] leading-relaxed text-muted-foreground">
            engrams drives GitHub through a <span className="font-medium">GitHub App</span> you own.
            Create it with the permissions and webhook below, install it on the org that owns your
            repos, then paste its App ID + private key here.
          </div>
        </div>
      </div>

      {/* Step 1 — create the app */}
      <section className="flex flex-col gap-2">
        <StepHeader n={1} label="Create a GitHub App" />
        <p className="text-[0.78rem] leading-relaxed text-muted-foreground">
          Open GitHub →{" "}
          <span className="font-mono">
            Settings → Developer settings → GitHub Apps → New GitHub App
          </span>
          , or use the pre-filled manifest to skip the permission wiring.
        </p>
        <div className="flex flex-wrap gap-2">
          <Button asChild variant="outline" size="sm">
            <a href={NEW_APP_URL} target="_blank" rel="noreferrer">
              Create GitHub App
              <ArrowUpRightIcon className="size-3.5" />
            </a>
          </Button>
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => copy("manifest", manifest)}
          >
            {copied === "manifest" ? (
              <CheckIcon className="size-3.5 text-instrument-nominal" />
            ) : (
              <CopyIcon className="size-3.5" />
            )}
            Copy App manifest
          </Button>
        </div>
        <p className="text-[0.72rem] text-muted-foreground">
          The manifest pre-fills every permission, the webhook URL, and the events — paste it into
          GitHub's <span className="italic">“Register a GitHub App from a manifest”</span> flow.
        </p>
      </section>

      {/* Step 2 — permissions */}
      <section className="flex flex-col gap-2">
        <StepHeader n={2} label="Grant repository permissions" />
        <div className="flex flex-col gap-2">
          <Text variant="label">Minimum (required)</Text>
          <div className="flex flex-col gap-1.5">
            {minimum.map((p) => (
              <PermRow key={p.key} permKey={p.key} level={p.level} />
            ))}
          </div>
          <p className="text-[0.72rem] text-muted-foreground">
            The floor for git + pull requests. Sessions can do no more than the App is granted.
          </p>
        </div>
        {extra.length > 0 && (
          <div className="mt-1 flex flex-col gap-2">
            <Text variant="label">Optional — unlock more powers</Text>
            <div className="flex flex-col gap-1.5">
              {extra.map((p) => (
                <PermRow key={p.key} permKey={p.key} level={p.level} />
              ))}
            </div>
            <p className="text-[0.72rem] text-muted-foreground">
              Add any of these to let profiles grant the matching{" "}
              <span className="font-mono">github:</span> powers (e.g. Actions, checks, deployments).
              Skip the ones you don't need.
            </p>
          </div>
        )}
      </section>

      {/* Step 3 — webhook */}
      <section className="flex flex-col gap-2">
        <StepHeader n={3} label="Add the webhook (for review-driven sessions)" />
        <p className="text-[0.78rem] leading-relaxed text-muted-foreground">
          So a review request or PR comment can trigger a session, point the App's webhook at this
          URL and subscribe to the events below. Set the webhook{" "}
          <span className="font-medium">secret</span> to the value your deployment configures (Helm{" "}
          <span className="font-mono">forge.github.webhookSecret</span>).
        </p>
        <div className="flex flex-col gap-1.5">
          <Text variant="label" className="flex items-center gap-1.5">
            <WebhookIcon className="size-3.5" />
            Payload URL
          </Text>
          <code className="select-all rounded-md border bg-card px-3 py-2 font-mono text-[0.72rem] break-all">
            {webhookUrl}
          </code>
          <div className="mt-1">
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => copy("url", webhookUrl)}
            >
              {copied === "url" ? (
                <CheckIcon className="size-3.5 text-instrument-nominal" />
              ) : (
                <CopyIcon className="size-3.5" />
              )}
              Copy payload URL
            </Button>
          </div>
        </div>
        <div className="flex flex-col gap-1.5">
          <Text variant="label">Subscribe to events</Text>
          <div className="flex flex-wrap gap-1.5">
            {WEBHOOK_EVENTS.map((e) => (
              <code
                key={e}
                className="rounded-full border bg-card px-2.5 py-0.5 font-mono text-[0.68rem] text-foreground"
              >
                {e}
              </code>
            ))}
          </div>
        </div>
      </section>

      {/* Step 4 — install + credentials */}
      <section className="flex flex-col gap-2">
        <StepHeader n={4} label="Install it & copy the credentials" />
        <p className="flex gap-2 text-[0.78rem] leading-relaxed text-muted-foreground">
          <KeyRoundIcon className="mt-0.5 size-3.5 shrink-0" />
          <span>
            Install the App on the org/user that owns your repos, then generate a private key. Copy
            the numeric <span className="font-medium">App ID</span> and the downloaded{" "}
            <span className="font-medium">.pem</span> into the fields below.
          </span>
        </p>
      </section>
    </div>
  );
}

function StepHeader({ n, label }: { n: number; label: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="inline-flex size-5 shrink-0 items-center justify-center rounded-full border border-primary bg-primary/20 font-mono text-[0.66rem] font-bold">
        {n}
      </span>
      <span className="text-sm font-semibold">{label}</span>
    </div>
  );
}

function PermRow({ permKey, level }: { permKey: string; level: "read" | "write" }) {
  return (
    <div className="flex items-center gap-2.5 rounded-md border bg-card px-3 py-1.5">
      <span className="flex-1 text-sm">{permissionLabel(permKey)}</span>
      <code className="font-mono text-xs text-muted-foreground">{permKey}</code>
      <AccessTag access={level === "write" ? "write" : "read"} />
    </div>
  );
}
