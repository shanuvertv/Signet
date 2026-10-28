//! TLS configuration.
//!
//! We deliberately use [`rustls_platform_verifier`] rather than a bundled root store. On Windows
//! that means the system certificate store, on Android the platform trust anchors — so a private
//! company CA installed through normal IT channels just works, and revocation follows the OS. A
//! bundled `webpki-roots` store would silently reject exactly the internal servers this client
//! exists to talk to.
//!
//! There is intentionally no way to disable verification. See PLAN.md §7.2.

use std::sync::{Arc, OnceLock};

use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio_rustls::TlsConnector;

use crate::{Error, Result};

static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

/// Process-wide client configuration, built once.
///
/// Constructing the platform verifier touches the OS trust store, which is slow enough on Windows
/// that doing it per connection is noticeable during a multi-account poll.
pub fn client_config() -> Result<Arc<ClientConfig>> {
    if let Some(cfg) = CONFIG.get() {
        return Ok(cfg.clone());
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?
        .with_platform_verifier()
        .map_err(|e| Error::Tls(e.to_string()))?
        .with_no_client_auth();

    let cfg = Arc::new(cfg);
    let _ = CONFIG.set(cfg.clone());
    Ok(cfg)
}

pub fn connector() -> Result<TlsConnector> {
    Ok(TlsConnector::from(client_config()?))
}
