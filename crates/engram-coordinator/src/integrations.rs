//! ADR 0056: the coordinator's registry of provider [`Integration`]s.
//!
//! Subsumes the single `state.forge: Option<Arc<dyn GitForge>>`. Keyed by
//! `Integration::provider()`, so the forge seam looks up `"github"` and a
//! future provider registers alongside it without new platform plumbing. The
//! broker holds the long-lived provider credentials (e.g. the GitHub App key);
//! the egress interceptor + connector config do the gating/injection/observation.

use std::collections::HashMap;
use std::sync::Arc;

use engram_core::traits::Integration;

/// Provider id → integration. Cheap to clone (Arc values).
#[derive(Clone, Default)]
pub struct IntegrationBroker {
    by_provider: HashMap<String, Arc<dyn Integration>>,
}

impl IntegrationBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a broker holding a single integration (the common one-provider
    /// case + tests).
    pub fn with(integration: Arc<dyn Integration>) -> Self {
        let mut b = Self::new();
        b.register(integration);
        b
    }

    /// Register (or replace) the integration for its `provider()`.
    pub fn register(&mut self, integration: Arc<dyn Integration>) {
        self.by_provider
            .insert(integration.provider().to_string(), integration);
    }

    /// The integration for `provider`, if configured.
    pub fn get(&self, provider: &str) -> Option<&Arc<dyn Integration>> {
        self.by_provider.get(provider)
    }

    /// Whether any integration is configured.
    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }
}
