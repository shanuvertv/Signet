//! A POP3 client (RFC 1939, plus `STLS` from RFC 2595, `UIDL`/`TOP` and `CAPA` from RFC 2449).
//!
//! Written by hand rather than taken from crates.io: the protocol is roughly ten verbs, and the
//! parts that actually matter to us — streaming `RETR` to disk, dot-unstuffing, the STARTTLS
//! injection check, and never committing a `DELE` without a clean `QUIT` — are exactly the parts
//! the thin wrapper crates get wrong or leave out.

mod conn;

use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::{Error, Result, tls};
use conn::{Conn, Transport};

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// TLS from the first byte. Port 995.
    ImplicitTls,
    /// Plaintext, then `STLS` before authenticating. Port 110.
    StartTls,
    /// No TLS at all. Lab use only — never offer this without an explicit warning in the UI.
    Plaintext,
}

#[derive(Debug, Clone)]
pub struct Pop3Config {
    pub host: String,
    pub port: u16,
    pub security: Security,
    /// Applies per network operation, not to the session as a whole.
    pub timeout: Duration,
}

impl Pop3Config {
    pub fn implicit_tls(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 995,
            security: Security::ImplicitTls,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn starttls(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 110,
            security: Security::StartTls,
            timeout: Duration::from_secs(30),
        }
    }

    /// Lab only. See [`Security::Plaintext`].
    pub fn plaintext(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 110,
            security: Security::Plaintext,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// One message as reported by `LIST` and `UIDL`.
///
/// `number` is only meaningful within the current session — servers renumber between sessions,
/// which is precisely why `uid` exists and why we key `pop_seen` on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    pub number: u32,
    pub size: u64,
    pub uid: Option<String>,
}

/// An authenticated POP3 session in the TRANSACTION state.
#[derive(Debug)]
pub struct Pop3Client {
    conn: Conn,
    capabilities: Vec<String>,
    greeting: String,
    authenticated: bool,
}

impl Pop3Client {
    /// Connect and negotiate transport security, leaving the session in AUTHORIZATION state.
    pub async fn connect(cfg: &Pop3Config) -> Result<Self> {
        let tcp = tokio::time::timeout(
            cfg.timeout,
            TcpStream::connect((cfg.host.as_str(), cfg.port)),
        )
        .await
        .map_err(|_| Error::Timeout(cfg.timeout))??;
        tcp.set_nodelay(true)?;

        let transport = if cfg.security == Security::ImplicitTls {
            Transport::Plain(tcp)
                .upgrade(&tls::connector()?, server_name(&cfg.host)?)
                .await?
        } else {
            Transport::Plain(tcp)
        };

        let mut conn = Conn::new(transport, cfg.timeout);
        let greeting = conn.read_status("greeting").await?;

        // CAPA predates nothing and postdates RFC 1939, so a refusal here is not fatal.
        let mut capabilities = read_capa(&mut conn).await.unwrap_or_default();

        if cfg.security == Security::StartTls {
            if !has_cap(&capabilities, "STLS") {
                return Err(Error::Unsupported("STLS"));
            }
            conn.command("STLS").await?;

            // RFC 2595 §4: everything received before the handshake must be discarded. If the
            // server pipelined anything, we cannot tell its output from an injected command.
            let pending = conn.pending();
            if pending > 0 {
                return Err(Error::StartTlsInjection(pending));
            }

            let (transport, timeout) = conn.into_parts();
            let transport = transport
                .upgrade(&tls::connector()?, server_name(&cfg.host)?)
                .await?;
            conn = Conn::new(transport, timeout);

            // Capabilities learned in the clear are unauthenticated; re-read them inside TLS.
            capabilities = read_capa(&mut conn).await.unwrap_or_default();
        }

        debug_assert_eq!(
            conn.is_encrypted(),
            cfg.security != Security::Plaintext,
            "encryption state must match the configured security mode"
        );

        Ok(Self {
            conn,
            capabilities,
            greeting,
            authenticated: false,
        })
    }

    /// The server's greeting line, minus `+OK`.
    pub fn greeting(&self) -> &str {
        &self.greeting
    }

    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        has_cap(&self.capabilities, cap)
    }

    pub fn is_encrypted(&self) -> bool {
        self.conn.is_encrypted()
    }

    /// Authenticate with `USER`/`PASS`, moving the session to TRANSACTION state.
    ///
    /// We refuse to send a password in the clear. A user who genuinely wants that must configure
    /// [`Security::Plaintext`] deliberately, which the UI surfaces as a warning.
    pub async fn login(&mut self, user: &str, password: &str, allow_plaintext: bool) -> Result<()> {
        if !self.conn.is_encrypted() && !allow_plaintext {
            return Err(Error::Unsupported(
                "TLS (refusing to send a password in the clear)",
            ));
        }
        self.conn.command(&format!("USER {user}")).await?;
        self.conn.command(&format!("PASS {password}")).await?;
        self.authenticated = true;
        // Post-authentication capabilities can differ from the pre-auth set.
        if let Ok(caps) = read_capa(&mut self.conn).await {
            self.capabilities = caps;
        }
        Ok(())
    }

    /// Message count and total maildrop size in octets.
    pub async fn stat(&mut self) -> Result<(u32, u64)> {
        let line = self.conn.command("STAT").await?;
        let mut parts = line.split_whitespace();
        let count = parse_field(parts.next(), "STAT count")?;
        let size = parse_field(parts.next(), "STAT size")?;
        Ok((count, size))
    }

    /// `LIST` joined with `UIDL`, which is what every caller actually wants.
    ///
    /// A server without `UIDL` yields `uid: None`; the sync layer then falls back to hashing
    /// headers and warns that duplicates are possible (PLAN.md §6).
    pub async fn list(&mut self) -> Result<Vec<MessageEntry>> {
        let sizes = self.list_sizes().await?;
        let uids = if self.has_capability("UIDL") {
            self.uidl().await.unwrap_or_default()
        } else {
            tracing::warn!("server does not advertise UIDL; duplicate detection will be weaker");
            Vec::new()
        };

        Ok(sizes
            .into_iter()
            .map(|(number, size)| {
                let uid = uids
                    .iter()
                    .find(|(n, _)| *n == number)
                    .map(|(_, u)| u.clone());
                MessageEntry { number, size, uid }
            })
            .collect())
    }

    async fn list_sizes(&mut self) -> Result<Vec<(u32, u64)>> {
        self.conn.command("LIST").await?;
        let mut body = Vec::new();
        self.conn.read_multiline_to(&mut body).await?;
        parse_pairs(&body, "LIST", |s| {
            s.parse::<u64>()
                .map_err(|_| Error::Protocol(format!("bad LIST size {s:?}")))
        })
    }

    pub async fn uidl(&mut self) -> Result<Vec<(u32, String)>> {
        self.conn.command("UIDL").await?;
        let mut body = Vec::new();
        self.conn.read_multiline_to(&mut body).await?;
        parse_pairs(&body, "UIDL", |s| Ok(s.to_string()))
    }

    /// Headers plus `lines` lines of body — how ingestion decides whether to fetch at all.
    pub async fn top(&mut self, number: u32, lines: u32) -> Result<Vec<u8>> {
        if !self.has_capability("TOP") {
            return Err(Error::Unsupported("TOP"));
        }
        self.conn.command(&format!("TOP {number} {lines}")).await?;
        let mut out = Vec::new();
        self.conn.read_multiline_to(&mut out).await?;
        Ok(out)
    }

    /// Stream a whole message into `out`, returning the byte count written.
    ///
    /// Nothing is buffered in memory beyond one line, so this is safe on messages far larger than
    /// available RAM.
    pub async fn retr_to<W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        number: u32,
        out: &mut W,
    ) -> Result<u64> {
        self.conn.command(&format!("RETR {number}")).await?;
        self.conn.read_multiline_to(out).await
    }

    /// Mark a message for deletion. **Not** committed until [`Pop3Client::quit`] succeeds.
    pub async fn dele(&mut self, number: u32) -> Result<()> {
        self.conn.command(&format!("DELE {number}")).await?;
        Ok(())
    }

    /// Undo every `DELE` issued this session.
    pub async fn rset(&mut self) -> Result<()> {
        self.conn.command("RSET").await?;
        Ok(())
    }

    pub async fn noop(&mut self) -> Result<()> {
        self.conn.command("NOOP").await?;
        Ok(())
    }

    /// Enter UPDATE state, committing pending deletions.
    ///
    /// Consuming `self` is deliberate: dropping the client without calling this is the one way to
    /// guarantee no deletion is committed, which is what every error path should do.
    pub async fn quit(mut self) -> Result<()> {
        self.conn.command("QUIT").await?;
        Ok(())
    }
}

fn has_cap(caps: &[String], want: &str) -> bool {
    caps.iter().any(|c| {
        c.split(' ')
            .next()
            .is_some_and(|v| v.eq_ignore_ascii_case(want))
    })
}

async fn read_capa(conn: &mut Conn) -> Result<Vec<String>> {
    conn.command("CAPA").await?;
    let mut body = Vec::new();
    conn.read_multiline_to(&mut body).await?;
    Ok(String::from_utf8_lossy(&body)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Parse the `<number> <value>` body shared by `LIST` and `UIDL`.
fn parse_pairs<T>(
    body: &[u8],
    ctx: &str,
    parse_value: impl Fn(&str) -> Result<T>,
) -> Result<Vec<(u32, T)>> {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.split_whitespace();
            let number: u32 = parse_field(parts.next(), ctx)?;
            let raw = parts
                .next()
                .ok_or_else(|| Error::Protocol(format!("{ctx} line {line:?} has no value")))?;
            Ok((number, parse_value(raw)?))
        })
        .collect()
}

fn parse_field<T: std::str::FromStr>(field: Option<&str>, ctx: &str) -> Result<T> {
    field
        .ok_or_else(|| Error::Protocol(format!("{ctx} missing")))?
        .parse()
        .map_err(|_| Error::Protocol(format!("{ctx} is not a number")))
}

fn server_name(host: &str) -> Result<ServerName<'static>> {
    ServerName::try_from(host.to_string()).map_err(|_| Error::InvalidDnsName(host.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_matching_ignores_arguments_and_case() {
        let caps = vec![
            "TOP".to_string(),
            "UIDL".to_string(),
            "SASL PLAIN LOGIN".to_string(),
            "IMPLEMENTATION Dovecot".to_string(),
        ];
        assert!(has_cap(&caps, "TOP"));
        assert!(has_cap(&caps, "uidl"));
        assert!(has_cap(&caps, "SASL"));
        assert!(!has_cap(&caps, "STLS"));
        // Must match the verb, not a substring of an argument.
        assert!(!has_cap(&caps, "PLAIN"));
        assert!(!has_cap(&caps, "Dovecot"));
    }

    #[test]
    fn parses_a_dovecot_uidl_body() {
        let body = b"1 GmlmZTAwMQ\r\n2 GmlmZTAwMg\r\n3 GmlmZTAwMw\r\n";
        let pairs = parse_pairs(body, "UIDL", |s| Ok(s.to_string())).unwrap();
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[1], (2, "GmlmZTAwMg".to_string()));
    }

    #[test]
    fn parses_list_sizes() {
        let body = b"1 4096\r\n2 120843\r\n";
        let pairs = parse_pairs(body, "LIST", |s| {
            s.parse::<u64>().map_err(|_| Error::Protocol("bad".into()))
        })
        .unwrap();
        assert_eq!(pairs, vec![(1, 4096), (2, 120843)]);
    }

    #[test]
    fn malformed_pair_lines_are_rejected_not_skipped() {
        // Silently dropping a line here would silently drop a message.
        let err = parse_pairs(b"1 4096\r\ngarbage\r\n", "LIST", |s| Ok(s.to_string())).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn config_defaults_match_the_conventional_ports() {
        assert_eq!(Pop3Config::implicit_tls("mail.example.com").port, 995);
        assert_eq!(Pop3Config::starttls("mail.example.com").port, 110);
    }
}
