//! SMTP submission, over `lettre`.
//!
//! Everything the app sends goes through the outbox rather than straight down a socket, so
//! composing while offline queues rather than fails. This module is only the transport half.

use std::time::Duration;

use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::pop3::Security;
use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub user: String,
    pub timeout: Duration,
}

impl SmtpConfig {
    pub fn new(host: impl Into<String>, port: u16, security: Security) -> Self {
        Self {
            host: host.into(),
            port,
            security,
            user: String::new(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// The transport both sending and testing use, so a successful test means the same settings that
/// will actually be used were the ones that worked.
fn transport_builder(
    config: &SmtpConfig,
    password: &str,
) -> Result<lettre::transport::smtp::AsyncSmtpTransportBuilder> {
    let tls = TlsParameters::new(config.host.clone()).map_err(|e| Error::Tls(e.to_string()))?;

    let builder = match config.security {
        Security::ImplicitTls => {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
                .port(config.port)
                .tls(Tls::Wrapper(tls))
        }
        Security::StartTls => {
            // `Required`, never `Opportunistic`: opportunistic STARTTLS is silently downgradeable
            // by anyone on the path, which defeats the point of using it.
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
                .port(config.port)
                .tls(Tls::Required(tls))
        }
        Security::Plaintext => {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
                .port(config.port)
                .tls(Tls::None)
        }
    };

    let builder = builder.timeout(Some(config.timeout));
    Ok(if config.user.is_empty() {
        builder
    } else {
        builder.credentials(Credentials::new(config.user.clone(), password.to_string()))
    })
}

/// Open a session and sign in, without sending anything.
///
/// Worth doing separately from the incoming check: the two halves of an account are configured
/// independently and fail independently, and an account whose incoming works while sending is
/// misconfigured looks perfectly healthy until the first reply will not go out.
pub async fn check_connection(config: &SmtpConfig, password: &str) -> Result<()> {
    let transport: AsyncSmtpTransport<Tokio1Executor> =
        transport_builder(config, password)?.build();
    transport
        .test_connection()
        .await
        .map_err(|e| map_smtp_error(&e))?;
    Ok(())
}

/// Send one already-built RFC822 message.
///
/// `envelope_from` and `envelope_to` are the SMTP envelope, which is deliberately separate from
/// the `From:`/`To:` headers in `raw` — Bcc recipients exist only in the envelope, and putting
/// them in a header would disclose them to every other recipient.
pub async fn send_raw(
    config: &SmtpConfig,
    password: &str,
    envelope_from: &str,
    envelope_to: &[String],
    raw: &[u8],
) -> Result<()> {
    if envelope_to.is_empty() {
        return Err(Error::Protocol("no recipients".into()));
    }

    let builder = transport_builder(config, password)?;

    let from = envelope_from
        .parse()
        .map_err(|_| Error::Protocol(format!("invalid sender address {envelope_from:?}")))?;
    let mut to = Vec::with_capacity(envelope_to.len());
    for addr in envelope_to {
        to.push(
            addr.parse()
                .map_err(|_| Error::Protocol(format!("invalid recipient address {addr:?}")))?,
        );
    }
    let envelope = lettre::address::Envelope::new(Some(from), to)
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let transport = builder.build();
    transport
        .send_raw(&envelope, raw)
        .await
        .map_err(|e| map_smtp_error(&e))?;
    Ok(())
}

/// Distinguish "try again later" from "this will never work".
///
/// The outbox retries the first and parks the second. Getting this backwards means either a
/// message that retries a permanent rejection forever, or one that gives up on a transient blip.
fn map_smtp_error(e: &lettre::transport::smtp::Error) -> Error {
    let message = e.to_string();
    if e.is_permanent() {
        Error::Rejected {
            command: "SMTP".to_string(),
            message,
        }
    } else {
        // Transient (4xx) and connection-level failures both warrant a retry.
        Error::Io(std::io::Error::other(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_to_send_with_no_recipients() {
        let cfg = SmtpConfig::new("mail.invalid", 587, Security::StartTls);
        let err = send_raw(&cfg, "pw", "a@b.c", &[], b"raw")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_malformed_addresses_before_connecting() {
        let cfg = SmtpConfig::new("mail.invalid", 587, Security::StartTls);
        let err = send_raw(&cfg, "pw", "not-an-address", &["x@y.z".into()], b"raw")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
    }
}
