//! The workspace's rustls transport seam.
//!
//! One job: guarantee that a rustls [`CryptoProvider`] is installed before
//! any HTTPS client exists, and make that guarantee impossible to forget by
//! owning client construction.
//!
//! # Why this crate exists
//!
//! The workspace compiles reqwest with `rustls-no-provider` (root manifest).
//! That feature brings the TLS stack but selects no crypto provider, which
//! is what keeps `aws-lc-rs` — a cmake+nasm C/asm build we would have to
//! carry through the musl cross-compile lane — out of the tree, and leaves
//! `ring` as the single compiled provider.
//!
//! The cost is that reqwest then requires a PROCESS-level provider.
//! `ClientBuilder::build` resolves it as
//!
//! ```text
//! CryptoProvider::get_default().unwrap_or_else(default_rustls_crypto_provider)
//! ```
//!
//! and under `rustls-no-provider` that fallback is an unconditional
//! `panic!("No rustls crypto provider is configured …")`. It does not fall
//! back to the compiled-in provider. So every client — HTTPS or plaintext,
//! since the TLS connector is built eagerly either way — panics until
//! something installs one.
//!
//! reqwest 0.12's `rustls-tls` did select a provider at compile time, so
//! this seam is new alongside the 0.13 move. It replaces what used to be an
//! inline install inside `OciClient::new`, which was correct but only
//! covered that one client.
//!
//! # Why construction lives here
//!
//! An `install_provider()` that every call site must remember to call is a
//! latent production panic: the compiler cannot check it, and the failure
//! surfaces at the first request rather than at build time. So this crate
//! exposes the BUILDER instead. Reach for [`client_builder`] (or [`client`])
//! and the provider is already installed; there is no supported way to get a
//! client from here without it.
//!
//! Transitive clients built inside SDKs we do not control (`gcloud-auth`,
//! `oci-client`, `kube`) read the same process-global provider, so they are
//! covered as soon as any of our own construction has happened — which is
//! why the constructors that wrap those SDKs call [`install_provider`]
//! directly before handing off.

/// Install `ring` as the process rustls provider.
///
/// Idempotent and safe to call from anywhere, including concurrently. The
/// workspace compiles exactly ONE provider, so this cannot pick a wrong one
/// and cannot race a different one: a second install of the same provider is
/// the only possible `Err`, which is why the result is dropped.
///
/// Prefer [`client_builder`]. Call this directly only when the client is
/// built by an SDK we do not control and so cannot be routed through this
/// crate.
pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A [`reqwest::ClientBuilder`] with the process provider already installed.
///
/// This is the workspace's entry point for building an HTTP client. Callers
/// keep full control of the builder — timeouts, pools, redirect policy — and
/// only give up the ability to forget the provider.
pub fn client_builder() -> reqwest::ClientBuilder {
    install_provider();
    reqwest::Client::builder()
}

/// A default [`reqwest::Client`], provider installed.
///
/// The stand-in for `reqwest::Client::new()`. Panics on the same conditions
/// `Client::new()` does — a TLS backend that cannot initialize — which is a
/// broken build rather than a runtime input.
pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("default reqwest client must build once a provider is installed")
}

#[cfg(test)]
mod tests {
    /// The contract this crate exists for: building a client must not panic
    /// in a process where nothing installed a provider first.
    ///
    /// This is the regression pin for the whole seam. Before it existed, the
    /// same failure showed up as 33 unrelated-looking test failures across
    /// six crates, each one a "No rustls crypto provider is configured"
    /// panic inside whatever client that crate happened to build.
    #[test]
    fn client_builds_without_a_preinstalled_provider() {
        let _ = super::client_builder()
            .build()
            .expect("client must build without a preinstalled provider");
    }

    /// A second install is a no-op, not a panic or an error escalation —
    /// which is what lets every construction site call it unconditionally.
    #[test]
    fn install_is_idempotent() {
        super::install_provider();
        super::install_provider();
        let _ = super::client();
    }
}
