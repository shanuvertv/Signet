//! Credential storage.
//!
//! Passwords never touch the SQLite file (PLAN.md §7.1). The database holds an opaque handle; the
//! secret itself lives in the platform keystore — Windows Credential Manager (DPAPI-backed) on the
//! desktop, and on Android a Keystore-wrapped key over EncryptedSharedPreferences.
//!
//! There is no OAuth here, by decision: ADR 0001 / D1 limits us to company and self-hosted servers, so
//! password auth over TLS is the only scheme v1 needs. The [`Credential`] enum still has room for
//! an `XOAuth2` variant so adding it later does not touch call sites.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("no credential stored for {0}")]
    NotFound(String),

    #[error("platform keystore error: {0}")]
    Keystore(String),

    /// Reached only on targets `keyring` has no store for. Windows and Android are both covered.
    #[error("credential storage is not implemented on this platform yet")]
    UnsupportedPlatform,
}

/// How an account authenticates.
///
/// A single-variant enum today, on purpose — see the module docs.
#[derive(Clone)]
pub enum Credential {
    Password(String),
}

impl std::fmt::Debug for Credential {
    /// Hand-written so a stray `{:?}` cannot put a secret in a log (PLAN.md §7.8).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Password(_) => f.write_str("Credential::Password(<redacted>)"),
        }
    }
}

impl Credential {
    pub fn expose(&self) -> &str {
        match self {
            Credential::Password(p) => p,
        }
    }
}

/// The keystore entry name for an account, e.g. `pop3:dev@example.com@mail.example.com`.
///
/// Stored in the accounts table so the secret can be found again without keeping it there.
pub fn handle_for(protocol: &str, user: &str, host: &str) -> String {
    format!("{protocol}:{user}@{host}")
}

/// The Credential Manager entry every saved password is filed under.
const SERVICE: &str = "Signet";


/// Store a secret, replacing any existing one for `handle`.
pub fn store(handle: &str, credential: &Credential) -> Result<()> {
    platform::store(SERVICE, handle, credential.expose())
}

/// Retrieve a secret.
pub fn load(handle: &str) -> Result<Credential> {
    platform::load(SERVICE, handle).map(Credential::Password)
}

/// Remove a secret. Succeeds if it was already absent.
pub fn delete(handle: &str) -> Result<()> {
    match platform::delete(SERVICE, handle) {
        Ok(()) | Err(Error::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Windows Credential Manager and the Android Keystore, both via `keyring` 4.
///
/// Verified on Windows (Phase 0 spike #3). **Android compiles but is not runtime-verified**: the
/// Keystore store needs a JNI context supplied by the host app, which does not exist until the
/// Tauri shell lands in Phase 6. Treat Android as unproven until that spike runs on a device.
#[cfg(any(windows, target_os = "android"))]
mod platform {
    use super::{Error, Result};
    use keyring::Entry;

    fn entry(service: &str, handle: &str) -> Result<Entry> {
        Entry::new(service, handle).map_err(|e| Error::Keystore(e.to_string()))
    }

    pub(super) fn store(service: &str, handle: &str, secret: &str) -> Result<()> {
        entry(service, handle)?
            .set_password(secret)
            .map_err(|e| Error::Keystore(e.to_string()))
    }

    pub(super) fn load(service: &str, handle: &str) -> Result<String> {
        entry(service, handle)?.get_password().map_err(|e| match e {
            keyring::Error::NoEntry => Error::NotFound(handle.to_string()),
            other => Error::Keystore(other.to_string()),
        })
    }

    pub(super) fn delete(service: &str, handle: &str) -> Result<()> {
        entry(service, handle)?.delete_credential().map_err(|e| match e {
            keyring::Error::NoEntry => Error::NotFound(handle.to_string()),
            other => Error::Keystore(other.to_string()),
        })
    }
}

#[cfg(not(any(windows, target_os = "android")))]
mod platform {
    use super::{Error, Result};

    pub(super) fn store(_service: &str, _handle: &str, _secret: &str) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    pub(super) fn load(_service: &str, _handle: &str) -> Result<String> {
        Err(Error::UnsupportedPlatform)
    }

    pub(super) fn delete(_service: &str, _handle: &str) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_unique_per_account_and_protocol() {
        let a = handle_for("pop3", "dev@example.com", "mail.example.com");
        let b = handle_for("smtp", "dev@example.com", "mail.example.com");
        let c = handle_for("pop3", "other@example.com", "mail.example.com");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn credentials_never_debug_the_secret() {
        let rendered = format!("{:?}", Credential::Password("hunter2".into()));
        assert!(!rendered.contains("hunter2"), "secret leaked: {rendered}");
    }

    /// Phase 0 spike #3. Touches the real Windows Credential Manager, so it is opt-in.
    #[cfg(windows)]
    #[test]
    #[ignore = "writes to the real Windows Credential Manager; run with --ignored"]
    fn windows_credential_manager_round_trip() {
        let handle = handle_for("pop3", "spike@example.com", "test.invalid");
        let secret = Credential::Password("phase-0-spike".into());

        store(&handle, &secret).expect("store");
        let loaded = load(&handle).expect("load");
        assert_eq!(loaded.expose(), "phase-0-spike");

        delete(&handle).expect("delete");
        assert!(matches!(load(&handle), Err(Error::NotFound(_))));
        // Deleting twice must not error.
        delete(&handle).expect("idempotent delete");
    }
}
