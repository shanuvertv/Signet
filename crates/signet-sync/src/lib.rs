//! Sync orchestration: the transport abstraction, the ingestion pipeline and the outbox.

pub mod imap_sync;
pub mod imap_transport;
pub mod ingest;
pub mod junk;
pub mod junk_signals;
pub mod outbox;
pub mod pop3_transport;
pub mod progress;
pub mod rules;
pub mod transport;

use std::time::Duration;

use signet_net::pop3::{Pop3Config, Security as NetSecurity};
use signet_store::{Account, Security as StoreSecurity, Store};
use thiserror::Error;

pub use ingest::SyncReport;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Net(#[from] signet_net::Error),

    #[error(transparent)]
    Store(signet_store::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("could not parse message: {0}")]
    Parse(String),

    #[error("transport is not connected")]
    NotConnected,

    #[error("no such folder on this server: {0}")]
    UnknownFolder(String),

    /// The caller referenced a message that was not in the last `list_messages` result. With POP3
    /// this usually means the maildrop changed underneath us and the session must be restarted.
    #[error("message {0} is not present in this session")]
    UnknownMessage(String),

    #[error("transport does not support {0}")]
    Unsupported(&'static str),
}

impl From<signet_store::Error> for Error {
    fn from(e: signet_store::Error) -> Self {
        Error::Store(e)
    }
}

impl Error {
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Net(e) => e.is_transient(),
            Error::Io(_) => true,
            // A vanished message means "resync", which is a retry at the session level.
            Error::NotConnected | Error::UnknownMessage(_) => true,
            Error::Store(_) | Error::Parse(_) | Error::UnknownFolder(_) | Error::Unsupported(_) => {
                false
            }
        }
    }
}

/// Translate the stored account settings into a POP3 connection config.
pub fn pop3_config_for(account: &Account) -> Pop3Config {
    let security = match account.pop_security {
        StoreSecurity::Tls => NetSecurity::ImplicitTls,
        StoreSecurity::Starttls => NetSecurity::StartTls,
        StoreSecurity::Plain => NetSecurity::Plaintext,
    };
    Pop3Config {
        host: account.pop_host.clone(),
        port: account.pop_port,
        security,
        timeout: Duration::from_secs(30),
    }
}

/// Poll one account, over whichever protocol it is configured for.
///
/// This is the dispatch point ADR 0001 / D3 was designed around: everything above it works in
/// terms of the transport, not the protocol.
pub async fn sync_account(
    store: &Store,
    account: &Account,
    password: &str,
) -> Result<ingest::SyncReport> {
    sync_account_reporting(store, account, password, &progress::Progressing::silent()).await
}

/// Sync, telling `progress` how it is getting on.
///
/// Separate from [`sync_account`] rather than an extra argument on it so that every caller which
/// does not want progress — the CLI, the tests, the outbox — stays exactly as it was.
pub async fn sync_account_reporting(
    store: &Store,
    account: &Account,
    password: &str,
    progress: &progress::Progressing,
) -> Result<ingest::SyncReport> {
    match account.protocol {
        signet_store::Protocol::Pop3 => ingest::sync_pop3(store, account, password, progress).await,
        signet_store::Protocol::Imap => {
            imap_sync::sync_account(store, account, password, progress).await
        }
    }
}

/// Translate the stored account settings into an IMAP connection config.
///
/// The `pop_*` columns hold the incoming-server settings for either protocol; `protocol` says how
/// to talk to them (migration 0005).
pub fn imap_config_for(account: &Account) -> signet_net::imap::ImapConfig {
    let security = match account.pop_security {
        StoreSecurity::Tls => NetSecurity::ImplicitTls,
        StoreSecurity::Starttls => NetSecurity::StartTls,
        StoreSecurity::Plain => NetSecurity::Plaintext,
    };
    signet_net::imap::ImapConfig {
        host: account.pop_host.clone(),
        port: account.pop_port,
        security,
        timeout: Duration::from_secs(30),
    }
}

/// Open an SMTP session and sign in, without sending anything.
///
/// Lives here rather than in the core so the dependency edge stays core -> sync -> net: the core
/// has no business knowing which SMTP library is underneath.
pub async fn check_smtp(account: &Account, password: &str) -> Result<()> {
    signet_net::smtp::check_connection(&smtp_config_for(account), password).await?;
    Ok(())
}

/// Translate the stored account settings into an SMTP submission config.
pub fn smtp_config_for(account: &Account) -> signet_net::smtp::SmtpConfig {
    let security = match account.smtp_security {
        StoreSecurity::Tls => NetSecurity::ImplicitTls,
        StoreSecurity::Starttls => NetSecurity::StartTls,
        StoreSecurity::Plain => NetSecurity::Plaintext,
    };
    let mut cfg =
        signet_net::smtp::SmtpConfig::new(account.smtp_host.clone(), account.smtp_port, security);
    cfg.user = account.smtp_user.clone();
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(sec: StoreSecurity, port: u16) -> Account {
        Account {
            id: 1,
            display_name: "T".into(),
            email: "t@x.test".into(),
            pop_host: "mail.x.test".into(),
            pop_port: port,
            pop_security: sec,
            pop_user: "t".into(),
            smtp_host: "mail.x.test".into(),
            smtp_port: 587,
            smtp_security: sec,
            smtp_user: "t".into(),
            credential_handle: "h".into(),
            leave_on_server: true,
            delete_after_days: None,
            poll_minutes: 5,
            signature: String::new(),
            is_enabled: true,
            last_sync_utc: None,
            junk_filter_enabled: true,
            junk_threshold: 0.9,
            protocol: signet_store::Protocol::Pop3,
        }
    }

    #[test]
    fn security_modes_map_through_to_the_network_layer() {
        assert_eq!(
            pop3_config_for(&account(StoreSecurity::Tls, 995)).security,
            NetSecurity::ImplicitTls
        );
        assert_eq!(
            pop3_config_for(&account(StoreSecurity::Starttls, 110)).security,
            NetSecurity::StartTls
        );
        // A stored account must never quietly become plaintext.
        assert_eq!(
            pop3_config_for(&account(StoreSecurity::Plain, 110)).security,
            NetSecurity::Plaintext
        );
    }

    #[test]
    fn the_configured_port_is_honoured_over_the_convention() {
        let cfg = pop3_config_for(&account(StoreSecurity::Tls, 9995));
        assert_eq!(cfg.port, 9995);
    }
}
