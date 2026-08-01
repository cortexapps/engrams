//! Host-side TLS-MITM proxy for Engram sandboxes.
//!
//! See `docs/known-issues.md` and the FC parity plan
//! (`/Users/nikhilunni/.claude/plans/...`) for the full design. In one
//! paragraph: every outbound TCP connection from a sandbox gets
//! REDIRECTed by iptables to this proxy. The proxy peeks the SNI,
//! looks up the source IP in a session registry, and decides whether
//! to **bypass** (allowed by `manifest.network.allow_hosts`,
//! splice through), **intercept** (a `[secrets.X]` allowlist matches
//! — terminate TLS, substitute the real value for the placeholder
//! the guest sees, re-encrypt to upstream), or **reject** (close).
//!
//! Phase A (this commit) lands the offline-testable infrastructure:
//! CA, leaf mint, SNI peek, allow-host matcher, session registry,
//! placeholder-leak detection. Phase B wires it into the FC backend
//! and adds the sudo-gated VM-level e2e test.

pub mod bypass;
pub mod ca;
pub mod cert_mint;
pub mod dns;
pub mod graphql;
pub mod inject;
pub mod intercept;
pub mod observe;
pub mod policy;
pub mod proxy;
pub mod registry;
pub mod replayed;
pub mod resolver;
pub mod sni;
pub mod substitute;
mod time_source;
pub mod violation;

pub use proxy::{Listeners, Proxy, ProxyConfig};
pub use resolver::{
    default_resolver, ResolveError, StaticResolver, SystemResolver, UpstreamResolver,
};

pub use ca::{Ca, CaError, CaSource, EnvCaSource, LocalDiskCaSource};
pub use cert_mint::{CertMint, MintError};
pub use graphql::{parse_request_body as parse_graphql_request, ParsedGraphql};
pub use observe::{ObserveSink, ObservedAsset, UrlFallback};
pub use policy::{HostList, HostSpec, ParseError as PolicyParseError};
pub use registry::{
    Decision, GraphqlMatch, GraphqlOperation, InjectEntry, InjectRefresher, ObserveEntry,
    RefreshableCred, RefreshedInject, Registry, RequestPolicy, SecretEntry, SessionState,
    SuccessRule,
};
pub use sni::{peek_sni, PeekError as SniPeekError};
pub use substitute::{scan_for_violation, substitute};
pub use violation::first_match as scan_for_placeholder;
