//! POP3 as a [`MailTransport`]: the degenerate case.
//!
//! One folder, no server-side flags, deferred deletion. Everything the trait offers beyond that is
//! either a no-op or an error, and the sync engine learns which from
//! [`TransportCapabilities`] rather than by knowing it is talking to POP3.

use async_trait::async_trait;
use signet_net::pop3::{MessageEntry, Pop3Client, Pop3Config};
use tokio::io::AsyncWrite;

use crate::transport::{
    Flag, FolderId, FolderKind, MailTransport, RemoteFolder, RemoteId, RemoteMessage,
    TransportCapabilities,
};
use crate::{Error, Result};

/// Credentials for a POP3 account.
///
/// Held only for the life of one session; the durable copy lives in the platform keystore
/// (see `signet-auth`).
#[derive(Clone)]
pub struct Pop3Credentials {
    pub user: String,
    pub password: String,
    /// Permit `USER`/`PASS` over an unencrypted link. Lab use only.
    pub allow_plaintext: bool,
}

impl std::fmt::Debug for Pop3Credentials {
    /// Hand-written so a stray `{:?}` cannot put a password in a log or a crash report.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pop3Credentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("allow_plaintext", &self.allow_plaintext)
            .finish()
    }
}

#[derive(Debug)]
pub struct Pop3Transport {
    config: Pop3Config,
    credentials: Pop3Credentials,
    client: Option<Pop3Client>,
    /// `UIDL` → session-local message number. Rebuilt every session, because the numbers are not
    /// stable across them.
    numbers: Vec<(RemoteId, u32)>,
}

impl Pop3Transport {
    pub fn new(config: Pop3Config, credentials: Pop3Credentials) -> Self {
        Self {
            config,
            credentials,
            client: None,
            numbers: Vec::new(),
        }
    }

    fn client(&mut self) -> Result<&mut Pop3Client> {
        self.client.as_mut().ok_or(Error::NotConnected)
    }

    fn number_for(&self, id: &RemoteId) -> Result<u32> {
        self.numbers
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, n)| *n)
            .ok_or_else(|| Error::UnknownMessage(id.0.clone()))
    }

    /// Derive a stable id for a server that does not support `UIDL`.
    ///
    /// Weaker than a real UID and duplicates remain possible, which is why
    /// [`Pop3Transport::list_messages`] warns when it has to do this.
    fn fallback_id(entry: &MessageEntry) -> RemoteId {
        RemoteId(format!("size:{}#num:{}", entry.size, entry.number))
    }
}

#[async_trait]
impl MailTransport for Pop3Transport {
    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            server_side_folders: false,
            server_side_flags: false,
            stable_ids: true, // via UIDL; degrades if the server lacks it
            push: false,
            partial_fetch: true, // TOP
            deferred_delete: true,
        }
    }

    async fn connect(&mut self) -> Result<()> {
        let mut client = Pop3Client::connect(&self.config).await?;
        client
            .login(
                &self.credentials.user,
                &self.credentials.password,
                self.credentials.allow_plaintext,
            )
            .await?;
        tracing::info!(
            host = %self.config.host,
            encrypted = client.is_encrypted(),
            greeting = %client.greeting(),
            "pop3 session established"
        );
        self.client = Some(client);
        Ok(())
    }

    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>> {
        Ok(vec![RemoteFolder {
            id: FolderId::inbox(),
            name: "Inbox".to_string(),
            kind: FolderKind::Inbox,
        }])
    }

    async fn list_messages(&mut self, folder: &FolderId) -> Result<Vec<RemoteMessage>> {
        require_inbox(folder)?;
        let entries = self.client()?.list().await?;

        let mut missing_uidl = 0usize;
        let mut numbers = Vec::with_capacity(entries.len());
        let mut messages = Vec::with_capacity(entries.len());

        for entry in &entries {
            let id = match &entry.uid {
                Some(uid) => RemoteId(uid.clone()),
                None => {
                    missing_uidl += 1;
                    Self::fallback_id(entry)
                }
            };
            numbers.push((id.clone(), entry.number));
            messages.push(RemoteMessage {
                id,
                size: entry.size,
                flags: Vec::new(), // POP3 has none, by construction
            });
        }

        if missing_uidl > 0 {
            tracing::warn!(
                missing_uidl,
                total = entries.len(),
                "server returned no UIDL for some messages; duplicate detection is degraded"
            );
        }

        self.numbers = numbers;
        Ok(messages)
    }

    async fn fetch_headers(&mut self, folder: &FolderId, id: &RemoteId) -> Result<Vec<u8>> {
        require_inbox(folder)?;
        let number = self.number_for(id)?;
        Ok(self.client()?.top(number, 0).await?)
    }

    async fn fetch_message(
        &mut self,
        folder: &FolderId,
        id: &RemoteId,
        out: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<u64> {
        require_inbox(folder)?;
        let number = self.number_for(id)?;
        Ok(self.client()?.retr_to(number, out).await?)
    }

    async fn set_flags(
        &mut self,
        _folder: &FolderId,
        _id: &RemoteId,
        _flags: &[Flag],
    ) -> Result<()> {
        // Not an error: the sync engine sets flags unconditionally and relies on
        // `capabilities().server_side_flags` to know they stay local.
        Ok(())
    }

    async fn delete(&mut self, folder: &FolderId, id: &RemoteId) -> Result<()> {
        require_inbox(folder)?;
        let number = self.number_for(id)?;
        self.client()?.dele(number).await?;
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        match self.client {
            // QUIT is what makes staged DELEs real.
            Some(client) => Ok(client.quit().await?),
            None => Ok(()),
        }
    }
}

fn require_inbox(folder: &FolderId) -> Result<()> {
    if folder == &FolderId::inbox() {
        Ok(())
    } else {
        Err(Error::UnknownFolder(folder.0.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> Pop3Transport {
        Pop3Transport::new(
            Pop3Config::implicit_tls("mail.example.com"),
            Pop3Credentials {
                user: "u".into(),
                password: "hunter2".into(),
                allow_plaintext: false,
            },
        )
    }

    #[test]
    fn capabilities_describe_pop3_honestly() {
        let caps = transport().capabilities();
        assert!(!caps.server_side_flags);
        assert!(!caps.server_side_folders);
        assert!(!caps.push);
        assert!(caps.partial_fetch);
        assert!(caps.deferred_delete);
        // The property the UI keys its multi-device warning off.
        assert!(caps.is_local_authoritative());
    }

    #[test]
    fn credentials_never_debug_the_password() {
        let rendered = format!("{:?}", transport());
        assert!(!rendered.contains("hunter2"), "password leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[tokio::test]
    async fn operations_before_connect_report_not_connected() {
        let mut t = transport();
        let err = t.list_messages(&FolderId::inbox()).await.unwrap_err();
        assert!(matches!(err, Error::NotConnected), "got {err:?}");
    }

    #[tokio::test]
    async fn non_inbox_folders_are_rejected() {
        let mut t = transport();
        let err = t
            .list_messages(&FolderId("Archive".into()))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::UnknownFolder(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn setting_flags_is_a_silent_no_op() {
        let mut t = transport();
        // Must succeed even though POP3 cannot store flags, and even before connecting.
        t.set_flags(&FolderId::inbox(), &RemoteId("x".into()), &[Flag::Seen])
            .await
            .unwrap();
    }
}
