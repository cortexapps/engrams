/**
 * Facet → wire conversion for the coordinator's redirect OAuth family
 * (ADR 0106 addendum). The facet was validated at the connector parse
 * boundary (host containment, bounded params/paths); this is a pure shape
 * mapping. Metadata field names go camelCase (facet) → snake_case (the
 * coordinator's driver vocabulary).
 */

import { create } from "@bufbuild/protobuf";
import type { Connector, OauthMetadataField } from "./registry.ts";
import {
  RedirectMetadataProbeSchema,
  RedirectMetadataSpecSchema,
  RedirectOauthSpecSchema,
  type RedirectOauthSpec,
} from "../gen/engram/app/v1/oauth_pb.ts";

const FIELD_WIRE_NAMES: Record<OauthMetadataField, string> = {
  accountId: "account_id",
  displayName: "display_name",
  workspaceId: "workspace_id",
  workspaceName: "workspace_name",
};

function wireMap(
  map: Partial<Record<OauthMetadataField, string>> | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [field, path] of Object.entries(map ?? {})) {
    if (typeof path === "string") out[FIELD_WIRE_NAMES[field as OauthMetadataField]] = path;
  }
  return out;
}

/** Build the wire spec for a connector's oauth facet. `extraParams` lets a
 * caller append flow extras (e.g. `prompt: "consent"` on a reconnect); facet
 * params win on collision so a connector's declared behavior is stable. */
export function redirectOauthSpec(
  connector: Connector,
  extraParams?: Record<string, string>,
): RedirectOauthSpec {
  const oauth = connector.oauth;
  if (!oauth) throw new Error(`connector "${connector.provider}" has no oauth facet`);
  const metadata = oauth.metadata
    ? create(RedirectMetadataSpecSchema, {
        fromTokenResponse: wireMap(oauth.metadata.fromTokenResponse),
        probe: oauth.metadata.probe
          ? create(RedirectMetadataProbeSchema, {
              method: oauth.metadata.probe.method ?? "GET",
              host: oauth.metadata.probe.host ?? connector.hosts[0] ?? "",
              path: oauth.metadata.probe.path,
              body: oauth.metadata.probe.body ?? "",
              map: wireMap(oauth.metadata.probe.map),
            })
          : undefined,
      })
    : undefined;
  return create(RedirectOauthSpecSchema, {
    authorizeUrl: oauth.authorizeUrl,
    tokenUrl: oauth.tokenUrl,
    scopes: [...oauth.scopes],
    scopeDelimiter: oauth.scopeDelimiter ?? "",
    extraAuthorizeParams: { ...(extraParams ?? {}), ...(oauth.extraAuthorizeParams ?? {}) },
    clientIdRef: oauth.clientIdRef,
    clientSecretRef: oauth.clientSecretRef,
    pkce: oauth.pkce ?? false,
    metadata,
  });
}

/**
 * ADR 0115 amendment: the USER-subject variant of a connector's redirect
 * spec. The org facet's `extraAuthorizeParams` are never inherited — they
 * often pin the ORG identity (Linear's `actor: "app"` makes tokens post as
 * the application), and a personal flow must act as the authorizing user.
 * The `userCredential.oauth` overrides supply user-flow scopes, the scopes
 * query param (Slack `user_scope`), the grant path (Slack `authed_user`),
 * user-flow authorize extras, and a metadata mapping.
 */
export function userRedirectOauthSpec(
  connector: Connector,
  extraParams?: Record<string, string>,
): RedirectOauthSpec {
  const oauth = connector.oauth;
  const mode = connector.userCredential?.oauth;
  if (!oauth || mode === undefined) {
    throw new Error(`connector "${connector.provider}" has no user-scoped OAuth flow`);
  }
  const overrides = mode === true ? {} : mode;
  const base = redirectOauthSpec(connector, extraParams);
  base.extraAuthorizeParams = { ...(extraParams ?? {}), ...(overrides.authorizeParams ?? {}) };
  if (overrides.scopes !== undefined) base.scopes = [...overrides.scopes];
  if (overrides.scopesParam !== undefined) base.scopesParam = overrides.scopesParam;
  if (overrides.grantPath !== undefined) base.grantPath = overrides.grantPath;
  if (overrides.metadata !== undefined) {
    base.metadata = create(RedirectMetadataSpecSchema, {
      fromTokenResponse: wireMap(overrides.metadata.fromTokenResponse),
    });
  }
  return base;
}
