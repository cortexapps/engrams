/**
 * GitHub App setup helpers for the connect flow — the GitHub analogue of
 * slackManifest.ts. A GitHub App needs three things the raw App ID + private key
 * don't convey, and which are easy to get wrong by hand: the right *permissions*,
 * an *install* on the org that owns the target repos, and — for review-driven
 * sessions — a *webhook*. This module derives the minimum permission set, maps
 * each granted power → the App permission it needs, lists the webhook events, and
 * builds the copy-paste App manifest that pre-fills all of it.
 *
 * The permission mapping mirrors `permissions_for_caps` in
 * `crates/engram-git-github/src/lib.rs` (the coordinator mints the per-session
 * installation token from the same rule): a capability action is
 * `<resource>:<level>`; the App permission key is the resource with
 * `pulls` → `pull_requests`, and the level is the highest granted.
 */

import type { ConnectorCapabilityView } from "@/components/integrations/useConnectorViews";
import { resourceLabel } from "./connectorModel";

export type PermLevel = "read" | "write";

/** A GitHub App permission, e.g. `{ key: "pull_requests", level: "write" }`. */
export interface GithubPermission {
  key: string;
  level: PermLevel;
}

/**
 * The floor every engrams GitHub App needs for the core git + pull-request flow
 * to work at all: fetch code, push branches, and open/review PRs. `metadata:read`
 * is mandatory — GitHub grants it implicitly to every App and requires it
 * alongside any repo permission. These are ALWAYS requested; granted powers only
 * *add* to them (an App can hold more permissions than any one session uses — the
 * coordinator clamps each session's minted token down to its bound powers).
 */
export const MINIMUM_PERMISSIONS: GithubPermission[] = [
  { key: "metadata", level: "read" },
  { key: "contents", level: "write" },
  { key: "pull_requests", level: "write" },
];

const RANK: Record<string, number> = { read: 1, write: 2, admin: 3 };

/** Resource namespace → App permission key (only `pulls` diverges). */
function permKeyFor(resource: string): string {
  return resource === "pulls" ? "pull_requests" : resource;
}

/**
 * The full App permission set for a connector: the minimum floor unioned with a
 * permission for every power the connector can grant, each at the highest level
 * any of its powers needs. This is the "add more to give sessions more powers"
 * surface — every capability a profile could grant shows up here as an App
 * permission the admin can add.
 */
export function permissionsForCapabilities(caps: ConnectorCapabilityView[]): GithubPermission[] {
  const level: Record<string, PermLevel> = {};
  for (const p of MINIMUM_PERMISSIONS) level[p.key] = p.level;
  for (const c of caps) {
    const resource = c.action.split(":")[0];
    if (!resource) continue;
    const key = permKeyFor(resource);
    const lvl: PermLevel = c.access === "write" ? "write" : "read";
    if (!level[key] || RANK[lvl]! > RANK[level[key]!]!) level[key] = lvl;
  }
  return Object.entries(level)
    .map(([key, lvl]) => ({ key, level: lvl }))
    .sort((a, b) => a.key.localeCompare(b.key));
}

/** Whether a permission is part of the always-required floor. */
export function isMinimumPermission(key: string): boolean {
  return MINIMUM_PERMISSIONS.some((p) => p.key === key);
}

/** A human label for an App permission key (`pull_requests` → "Pull requests"). */
export function permissionLabel(key: string): string {
  return resourceLabel(key === "pull_requests" ? "pulls" : key);
}

/**
 * Webhook events a review-driven session subscribes to: a review requested on a
 * PR (`pull_request` action `review_requested`), a submitted review, and the two
 * comment surfaces agents reply through. Kept in lockstep with the manifest so
 * the "subscribe to these events" checklist and the pre-filled App never drift.
 */
export const WEBHOOK_EVENTS = [
  "pull_request",
  "pull_request_review",
  "pull_request_review_comment",
  "issue_comment",
];

/**
 * Where GitHub should POST webhook deliveries — the orchestrator's
 * `/api/v1/integrations/<provider>/events` route (the same convention Slack's
 * event webhook uses). Its verifying secret is set on the deployment (Helm
 * `forge.github.webhookSecret`), so the same value goes in the App's webhook
 * config — that Helm-side secret is what this onboarding surfaces in-product.
 */
export function githubWebhookUrl(origin: string): string {
  return `${origin}/api/v1/integrations/github/events`;
}

/** The engrams App as it appears in the target org. */
const APP_NAME = "engrams";

export interface GithubManifestInput {
  /** `window.location.origin` — the public base URL the webhook route lives under. */
  origin: string;
  /** The App permissions to request (minimum ∪ power-derived). */
  permissions: GithubPermission[];
  /** The webhook events to subscribe to. */
  events: string[];
}

/**
 * A GitHub App manifest for the "Register a GitHub App from a manifest" flow
 * (https://docs.github.com/apps/sharing-github-apps/registering-a-github-app-from-a-manifest).
 * Pre-fills the three error-prone things — permissions, the webhook (URL +
 * active), and the events — so the admin only names the App, installs it, and
 * pastes back the App ID + generated private key. Emitted as JSON via
 * `JSON.stringify`, so it's always valid.
 */
export function buildGithubManifest({ origin, permissions, events }: GithubManifestInput): string {
  const default_permissions: Record<string, PermLevel> = {};
  for (const p of permissions) default_permissions[p.key] = p.level;
  const manifest = {
    name: APP_NAME,
    url: origin,
    hook_attributes: { url: githubWebhookUrl(origin), active: true },
    redirect_url: `${origin}/settings/integrations/github`,
    public: false,
    default_permissions,
    default_events: events,
  };
  return JSON.stringify(manifest, null, 2);
}
