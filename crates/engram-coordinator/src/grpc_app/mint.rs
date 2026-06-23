//! ADR 0057 C3: `MintService` over app-gRPC — the read-only mint-kind registry.
//! The caller is the trusted orchestrator (bearer-authed); per-user authz
//! (admin-only) lives there. This exposes only the Plane-A *form metadata* from
//! the coordinator's `mint_kind_registry()`; the mint *logic* (building engines)
//! stays bespoke Rust in [`crate::integrations`].

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::auth;

pub struct AppMintService {
    pub auth: Arc<auth::BearerAuth>,
}

fn field_kind_to_proto(k: engram_core::traits::MintFieldKind) -> i32 {
    let v = match k {
        engram_core::traits::MintFieldKind::Config => app::MintFieldKind::Config,
        engram_core::traits::MintFieldKind::SealedSecret => app::MintFieldKind::SealedSecret,
    };
    v as i32
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::mint_service_server::MintService for AppMintService {
    async fn list_mint_kinds(
        &self,
        req: Request<app::ListMintKindsRequest>,
    ) -> Result<Response<app::ListMintKindsResponse>, Status> {
        self.auth.check(&req)?;
        let mint_kinds = crate::integrations::mint_kind_registry()
            .into_iter()
            .map(|d| app::MintKind {
                kind: d.kind.to_string(),
                provider: d.provider.to_string(),
                display_name: d.display_name.to_string(),
                fields: d
                    .fields
                    .iter()
                    .map(|f| app::MintFieldDescriptor {
                        name: f.name.to_string(),
                        label: f.label.to_string(),
                        field_kind: field_kind_to_proto(f.field_kind),
                        required: f.required,
                    })
                    .collect(),
            })
            .collect();
        Ok(Response::new(app::ListMintKindsResponse { mint_kinds }))
    }
}
