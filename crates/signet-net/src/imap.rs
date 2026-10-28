//! IMAP client.
//!
//! Unlike POP3 — ten verbs, hand-written here — IMAP is a large protocol with literals, untagged
//! responses and a parser that is easy to get subtly wrong. This wraps [`async_imap`], which is
//! maintained by the Delta Chat team and exercised against a wide range of real servers. What we
//! own is the part that matters to us: the connection and security policy, the mapping onto our
//! own types, and the UIDVALIDITY discipline.
//!
//! **UIDs, never sequence numbers.** Sequence numbers shift whenever anything is expunged, so a
//! client that stores them will eventually act on the wrong message. Every command here is a `UID`
//! command, and every id we persist is a UID paired with the folder's UIDVALIDITY.

use std::time::Duration;

use async_imap::Session;
use async_imap::types::Fetch;
use futures::StreamExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::pop3::Security;
use crate::{Error, Result};

/// The stream an IMAP session runs over.
///
/// An enum rather than TLS-only because a plaintext lab mode has to exist for the same reason it
/// does in POP3: local test servers (GreenMail, Dovecot in a container) present self-signed
/// certificates that the platform verifier rightly refuses, and disabling verification to work
/// around that would be far worse than an explicit, clearly-labelled plaintext opt-in.
#[derive(Debug)]
pub enum ImapStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl tokio::io::AsyncRead for ImapStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            ImapStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ImapStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ImapStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            ImapStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            ImapStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            ImapStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub timeout: Duration,
}

impl ImapConfig {
    pub fn implicit_tls(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 993,
            security: Security::ImplicitTls,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn starttls(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 143,
            security: Security::StartTls,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// A mailbox as the server describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// Full server path, e.g. `INBOX.Archive`. This is the identifier, not the display name.
    pub path: String,
    /// Last path segment, for display.
    pub name: String,
    /// RFC 6154 SPECIAL-USE attribute, e.g. `\Sent`, when the server offers one.
    pub special_use: Option<String>,
    /// True when the server says this mailbox holds no messages, only child mailboxes.
    pub selectable: bool,
}

/// One message's server-side summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapMessage {
    pub uid: u32,
    pub size: u64,
    pub flags: Vec<String>,
}

/// An authenticated IMAP session.
pub struct ImapClient {
    session: Session<ImapStream>,
    /// Currently selected mailbox and its UIDVALIDITY.
    selected: Option<(String, u32)>,
    capabilities: Vec<String>,
}

impl std::fmt::Debug for ImapClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapClient")
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl ImapClient {
    /// Connect, negotiate TLS, and log in.
    ///
    /// `allow_plaintext` must be set explicitly for [`Security::Plaintext`] to be accepted, so
    /// an account cannot be mis-configured into sending its password in the clear by picking the
    /// wrong dropdown entry alone.
    pub async fn connect(
        config: &ImapConfig,
        user: &str,
        password: &str,
        allow_plaintext: bool,
    ) -> Result<Self> {
        let tcp = tokio::time::timeout(
            config.timeout,
            TcpStream::connect((config.host.as_str(), config.port)),
        )
        .await
        .map_err(|_| Error::Timeout(config.timeout))??;
        tcp.set_nodelay(true)?;

        let server = ServerName::try_from(config.host.clone())
            .map_err(|_| Error::InvalidDnsName(config.host.clone()))?;
        let connector = crate::tls::connector()?;

        let stream = match config.security {
            Security::ImplicitTls => ImapStream::Tls(Box::new(
                connector
                    .connect(server, tcp)
                    .await
                    .map_err(|e| Error::Tls(e.to_string()))?,
            )),
            Security::StartTls => ImapStream::Tls(Box::new(
                starttls(tcp, server, &connector, config.timeout).await?,
            )),
            Security::Plaintext => {
                if !allow_plaintext {
                    return Err(Error::Unsupported(
                        "TLS (refusing to send an IMAP password in the clear)",
                    ));
                }
                tracing::warn!(
                    "connecting to IMAP WITHOUT encryption - credentials cross the network in the clear"
                );
                ImapStream::Plain(tcp)
            }
        };

        let client = async_imap::Client::new(stream);
        let mut session = client.login(user, password).await.map_err(|(e, _)| {
            // The tuple's second element is the un-consumed client; dropping it closes the socket.
            Error::Rejected {
                command: "LOGIN".to_string(),
                message: e.to_string(),
            }
        })?;

        let capabilities = match session.capabilities().await {
            Ok(caps) => caps.iter().map(|c| format!("{c:?}")).collect(),
            Err(e) => {
                tracing::debug!(error = %e, "CAPABILITY failed; continuing without it");
                Vec::new()
            }
        };

        Ok(Self {
            session,
            selected: None,
            capabilities,
        })
    }

    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    pub fn has_capability(&self, want: &str) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.to_uppercase().contains(&want.to_uppercase()))
    }

    /// Every selectable mailbox on the server.
    pub async fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        let mut out = Vec::new();
        {
            let mut stream = self
                .session
                .list(Some(""), Some("*"))
                .await
                .map_err(imap_err("LIST"))?;

            while let Some(item) = stream.next().await {
                let name = item.map_err(imap_err("LIST"))?;
                let path = name.name().to_string();
                let attrs: Vec<String> = name
                    .attributes()
                    .iter()
                    .map(|a| format!("{a:?}").to_lowercase())
                    .collect();

                let selectable = !attrs.iter().any(|a| a.contains("noselect"));
                let special_use = attrs
                    .iter()
                    .find(|a| {
                        [
                            "sent", "drafts", "trash", "junk", "archive", "all", "flagged",
                        ]
                        .iter()
                        .any(|k| a.contains(k))
                    })
                    .cloned();

                let delimiter = name.delimiter().unwrap_or(".");
                let display = path.rsplit(delimiter).next().unwrap_or(&path).to_string();

                out.push(Mailbox {
                    path,
                    name: display,
                    special_use,
                    selectable,
                });
            }
        }
        Ok(out)
    }

    /// Select a mailbox, returning its UIDVALIDITY.
    ///
    /// The caller must compare this with what it stored: a change means every UID it holds for
    /// this mailbox refers to nothing, and the folder has to be resynced.
    pub async fn select(&mut self, path: &str) -> Result<u32> {
        if let Some((current, validity)) = &self.selected {
            if current == path {
                return Ok(*validity);
            }
        }
        let mailbox = self
            .session
            .select(path)
            .await
            .map_err(imap_err("SELECT"))?;
        let validity = mailbox.uid_validity.unwrap_or(0);
        self.selected = Some((path.to_string(), validity));
        Ok(validity)
    }

    /// UID, size and flags for every message in a mailbox.
    pub async fn list_messages(&mut self, path: &str) -> Result<Vec<ImapMessage>> {
        self.select(path).await?;

        let mut out = Vec::new();
        {
            // `1:*` in a UID FETCH is the whole mailbox regardless of sequence numbers.
            let mut stream = self
                .session
                .uid_fetch("1:*", "(UID RFC822.SIZE FLAGS)")
                .await
                .map_err(imap_err("UID FETCH"))?;

            while let Some(item) = stream.next().await {
                let fetch = item.map_err(imap_err("UID FETCH"))?;
                let Some(uid) = fetch.uid else { continue };
                out.push(ImapMessage {
                    uid,
                    size: fetch.size.unwrap_or(0) as u64,
                    flags: flags_of(&fetch),
                });
            }
        }
        Ok(out)
    }

    /// Headers only. `BODY.PEEK` so that reading them does not mark the message seen for everyone.
    pub async fn fetch_headers(&mut self, path: &str, uid: u32) -> Result<Vec<u8>> {
        self.select(path).await?;
        let mut out = Vec::new();
        {
            let mut stream = self
                .session
                .uid_fetch(uid.to_string(), "(BODY.PEEK[HEADER])")
                .await
                .map_err(imap_err("UID FETCH HEADER"))?;
            while let Some(item) = stream.next().await {
                let fetch = item.map_err(imap_err("UID FETCH HEADER"))?;
                if let Some(body) = fetch.header() {
                    out.extend_from_slice(body);
                }
            }
        }
        Ok(out)
    }

    /// The whole message, written to `out`.
    ///
    /// `BODY.PEEK[]` rather than `BODY[]`: on a shared mailbox, one person opening a message must
    /// not silently mark it read for the entire team.
    pub async fn fetch_message<W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        path: &str,
        uid: u32,
        out: &mut W,
    ) -> Result<u64> {
        self.select(path).await?;
        let mut written = 0u64;
        {
            let mut stream = self
                .session
                .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
                .await
                .map_err(imap_err("UID FETCH BODY"))?;
            while let Some(item) = stream.next().await {
                let fetch = item.map_err(imap_err("UID FETCH BODY"))?;
                if let Some(body) = fetch.body() {
                    out.write_all(body).await?;
                    written += body.len() as u64;
                }
            }
        }
        out.flush().await?;
        Ok(written)
    }

    /// Add or remove flags on the server, where the whole team sees them.
    pub async fn store_flags(
        &mut self,
        path: &str,
        uid: u32,
        flags: &[&str],
        add: bool,
    ) -> Result<()> {
        if flags.is_empty() {
            return Ok(());
        }
        self.select(path).await?;
        let query = format!(
            "{}FLAGS.SILENT ({})",
            if add { "+" } else { "-" },
            flags.join(" ")
        );
        let stream = self
            .session
            .uid_store(uid.to_string(), query)
            .await
            .map_err(imap_err("UID STORE"))?;
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            item.map_err(imap_err("UID STORE"))?;
        }
        Ok(())
    }

    /// Move a message to another mailbox.
    ///
    /// Prefers RFC 6851 `UID MOVE`, which is atomic. Without it, falls back to copy-then-delete,
    /// which can leave a duplicate if the connection dies between the two — so the fallback marks
    /// deleted but does not expunge, leaving the evidence rather than destroying it.
    pub async fn move_message(&mut self, path: &str, uid: u32, target: &str) -> Result<()> {
        self.select(path).await?;

        if self.has_capability("MOVE") {
            return self
                .session
                .uid_mv(uid.to_string(), target)
                .await
                .map_err(imap_err("UID MOVE"));
        }

        self.session
            .uid_copy(uid.to_string(), target)
            .await
            .map_err(imap_err("UID COPY"))?;
        self.store_flags(path, uid, &["\\Deleted"], true).await
    }

    /// Mark deleted and expunge — the only path that actually destroys a message.
    pub async fn delete_message(&mut self, path: &str, uid: u32) -> Result<()> {
        self.store_flags(path, uid, &["\\Deleted"], true).await?;
        // UID EXPUNGE removes only what we just marked; plain EXPUNGE would also remove anything a
        // colleague had marked deleted but not yet expunged.
        if self.has_capability("UIDPLUS") {
            let stream = self
                .session
                .uid_expunge(uid.to_string())
                .await
                .map_err(imap_err("UID EXPUNGE"))?;
            let mut stream = Box::pin(stream);
            while let Some(item) = stream.next().await {
                item.map_err(imap_err("UID EXPUNGE"))?;
            }
            return Ok(());
        }
        let stream = self.session.expunge().await.map_err(imap_err("EXPUNGE"))?;
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            item.map_err(imap_err("EXPUNGE"))?;
        }
        Ok(())
    }

    /// Upload a message into a mailbox, for filing a sent copy server-side.
    pub async fn append(&mut self, path: &str, raw: &[u8], flags: &[&str]) -> Result<()> {
        let flag_list = (!flags.is_empty()).then(|| format!("({})", flags.join(" ")));
        self.session
            .append(path, flag_list.as_deref(), None, raw)
            .await
            .map_err(imap_err("APPEND"))
    }

    pub async fn logout(mut self) -> Result<()> {
        self.session.logout().await.map_err(imap_err("LOGOUT"))
    }
}

/// Run the STARTTLS handshake by hand and hand back the encrypted stream.
///
/// `async-imap` has no helper for this when it is not driving TLS itself, and doing it here keeps
/// the property the POP3 client also enforces: **nothing may be buffered when the handshake
/// begins.** A server that pipelined a response before TLS started is either badly broken or an
/// attacker injecting plaintext we would otherwise attribute to the encrypted session.
async fn starttls(
    mut tcp: TcpStream,
    server: ServerName<'static>,
    connector: &tokio_rustls::TlsConnector,
    timeout: Duration,
) -> Result<TlsStream<TcpStream>> {
    use tokio::io::AsyncReadExt;

    let mut buf: Vec<u8> = Vec::with_capacity(1024);

    // Read one CRLF-terminated line out of `buf`, filling from the socket as needed.
    async fn line(tcp: &mut TcpStream, buf: &mut Vec<u8>, timeout: Duration) -> Result<String> {
        loop {
            if let Some(idx) = buf.windows(2).position(|w| w == b"\r\n") {
                let out = String::from_utf8_lossy(&buf[..idx]).into_owned();
                buf.drain(..idx + 2);
                return Ok(out);
            }
            if buf.len() > 64 * 1024 {
                return Err(Error::Protocol("IMAP greeting line too long".into()));
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(timeout, tcp.read(&mut chunk))
                .await
                .map_err(|_| Error::Timeout(timeout))??;
            if n == 0 {
                return Err(Error::Closed);
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    // Server greeting.
    let greeting = line(&mut tcp, &mut buf, timeout).await?;
    if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
        return Err(Error::Protocol(format!(
            "unexpected greeting: {greeting:?}"
        )));
    }

    tcp.write_all(b"S1 STARTTLS\r\n").await?;
    tcp.flush().await?;

    loop {
        let response = line(&mut tcp, &mut buf, timeout).await?;
        if let Some(rest) = response.strip_prefix("S1 ") {
            if !rest.to_uppercase().starts_with("OK") {
                return Err(Error::Rejected {
                    command: "STARTTLS".to_string(),
                    message: rest.to_string(),
                });
            }
            break;
        }
        // Untagged responses before the tagged one are normal; keep reading.
    }

    if !buf.is_empty() {
        return Err(Error::StartTlsInjection(buf.len()));
    }

    connector
        .connect(server, tcp)
        .await
        .map_err(|e| Error::Tls(e.to_string()))
}

fn flags_of(fetch: &Fetch) -> Vec<String> {
    fetch.flags().map(|f| format!("{f:?}")).collect()
}

/// Turn an `async_imap` error into ours, keeping the command that produced it.
fn imap_err(command: &'static str) -> impl Fn(async_imap::error::Error) -> Error {
    move |e| match &e {
        // A `NO`/`BAD` from the server is the server's decision, not a transport problem, so it
        // must not be retried as though it were a blip.
        async_imap::error::Error::No(msg) | async_imap::error::Error::Bad(msg) => Error::Rejected {
            command: command.to_string(),
            message: msg.to_string(),
        },
        _ => Error::Protocol(format!("{command}: {e}")),
    }
}

/// Map a mailbox onto the folder role it plays.
///
/// SPECIAL-USE (RFC 6154) is authoritative when the server offers it. Otherwise fall back to the
/// names servers conventionally use — which is most of them, and is why the list is long.
pub fn classify(mailbox: &Mailbox) -> &'static str {
    if let Some(attr) = &mailbox.special_use {
        if attr.contains("sent") {
            return "sent";
        }
        if attr.contains("draft") {
            return "drafts";
        }
        if attr.contains("trash") {
            return "deleted";
        }
        if attr.contains("junk") {
            return "junk";
        }
        if attr.contains("archive") || attr.contains("all") {
            return "archive";
        }
    }

    let name = mailbox
        .path
        .rsplit(['.', '/'])
        .next()
        .unwrap_or(&mailbox.path)
        .to_lowercase();

    match name.as_str() {
        "inbox" => "inbox",
        "sent" | "sent items" | "sent messages" | "sentmail" | "sent-mail" => "sent",
        "drafts" | "draft" => "drafts",
        "trash" | "deleted" | "deleted items" | "bin" => "deleted",
        "junk" | "spam" | "junk e-mail" | "junk email" | "bulk mail" => "junk",
        "archive" | "archives" | "all mail" => "archive",
        _ => "custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(path: &str, special: Option<&str>) -> Mailbox {
        Mailbox {
            path: path.to_string(),
            name: path.rsplit('.').next().unwrap_or(path).to_string(),
            special_use: special.map(str::to_string),
            selectable: true,
        }
    }

    #[test]
    fn special_use_wins_over_the_name() {
        // A server that says "\Sent" is right even if the folder is called something else.
        assert_eq!(
            classify(&mailbox("INBOX.Verzonden", Some("\\sent"))),
            "sent"
        );
        assert_eq!(
            classify(&mailbox("INBOX.Papierkorb", Some("\\trash"))),
            "deleted"
        );
    }

    #[test]
    fn common_folder_names_are_recognised_without_special_use() {
        assert_eq!(classify(&mailbox("INBOX", None)), "inbox");
        assert_eq!(classify(&mailbox("INBOX.Sent", None)), "sent");
        assert_eq!(classify(&mailbox("INBOX.Sent Items", None)), "sent");
        assert_eq!(classify(&mailbox("INBOX.Drafts", None)), "drafts");
        assert_eq!(classify(&mailbox("INBOX.Trash", None)), "deleted");
        assert_eq!(classify(&mailbox("INBOX.Junk", None)), "junk");
        assert_eq!(classify(&mailbox("INBOX.Spam", None)), "junk");
        assert_eq!(classify(&mailbox("INBOX.Archive", None)), "archive");
    }

    #[test]
    fn matching_is_case_insensitive_and_ignores_the_parent_path() {
        assert_eq!(classify(&mailbox("INBOX.SENT", None)), "sent");
        assert_eq!(classify(&mailbox("Other/Deep/Trash", None)), "deleted");
    }

    #[test]
    fn an_ordinary_folder_stays_custom() {
        assert_eq!(classify(&mailbox("INBOX.Customers", None)), "custom");
        assert_eq!(classify(&mailbox("INBOX.2024 Quotes", None)), "custom");
    }

    #[test]
    fn conventional_ports_follow_the_security_mode() {
        assert_eq!(ImapConfig::implicit_tls("mail.test").port, 993);
        assert_eq!(ImapConfig::starttls("mail.test").port, 143);
    }
}
