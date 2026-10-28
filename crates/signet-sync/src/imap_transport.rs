//! IMAP as a [`MailTransport`]: the full case.
//!
//! Where `pop3_transport` implements a degenerate version of the trait — one folder, no flags,
//! deferred deletion — this implements what the trait was actually modelled on. That is the payoff
//! from ADR 0001 / D3: the sync engine and the UI did not need to learn a new shape.
//!
//! For a shared sales mailbox this is the answer to nearly every limitation in
//! `docs/team-workflow.md`: read state, flags and folders live on the server, so the whole team
//! sees the same thing without a shared file.

use async_trait::async_trait;
use signet_net::imap::{ImapClient, ImapConfig, classify};
use tokio::io::AsyncWrite;

use crate::transport::{
    Flag, FolderId, FolderKind, MailTransport, RemoteFolder, RemoteId, RemoteMessage,
    TransportCapabilities,
};
use crate::{Error, Result};

/// Credentials for an IMAP account. Held only for the life of one session.
#[derive(Clone)]
pub struct ImapCredentials {
    pub user: String,
    pub password: String,
    /// Permit an unencrypted connection. Lab use only — see [`signet_net::imap::ImapClient::connect`].
    pub allow_plaintext: bool,
}

impl std::fmt::Debug for ImapCredentials {
    /// Hand-written so a stray `{:?}` cannot put a password in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapCredentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("allow_plaintext", &self.allow_plaintext)
            .finish()
    }
}

#[derive(Debug)]
pub struct ImapTransport {
    config: ImapConfig,
    credentials: ImapCredentials,
    client: Option<ImapClient>,
    /// UIDVALIDITY seen for each mailbox this session, so the sync engine can detect a reset.
    validities: Vec<(String, u32)>,
}

impl ImapTransport {
    pub fn new(config: ImapConfig, credentials: ImapCredentials) -> Self {
        Self {
            config,
            credentials,
            client: None,
            validities: Vec::new(),
        }
    }

    fn client(&mut self) -> Result<&mut ImapClient> {
        self.client.as_mut().ok_or(Error::NotConnected)
    }

    /// UIDVALIDITY observed for a mailbox during this session.
    pub fn uid_validity(&self, path: &str) -> Option<u32> {
        self.validities
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, v)| *v)
    }

    /// Push local read/flagged/answered state to the server, where the team sees it.
    pub async fn push_flags(
        &mut self,
        folder: &FolderId,
        id: &RemoteId,
        read: bool,
        flagged: bool,
        answered: bool,
    ) -> Result<()> {
        let uid = parse_uid(id)?;
        let path = folder.0.clone();

        // Two STOREs rather than one: the set to add and the set to remove are both meaningful,
        // and sending only additions would never let anyone mark something unread again.
        let mut add: Vec<&str> = Vec::new();
        let mut remove: Vec<&str> = Vec::new();
        for (on, flag) in [
            (read, "\\Seen"),
            (flagged, "\\Flagged"),
            (answered, "\\Answered"),
        ] {
            if on {
                add.push(flag)
            } else {
                remove.push(flag)
            }
        }

        let client = self.client()?;
        client.store_flags(&path, uid, &add, true).await?;
        client.store_flags(&path, uid, &remove, false).await?;
        Ok(())
    }

    /// Move a message to another mailbox on the server.
    pub async fn move_to(&mut self, folder: &FolderId, id: &RemoteId, target: &str) -> Result<()> {
        let uid = parse_uid(id)?;
        let path = folder.0.clone();
        self.client()?.move_message(&path, uid, target).await?;
        Ok(())
    }

    /// File a sent copy in the server's Sent mailbox, so it appears for everyone.
    pub async fn append(&mut self, path: &str, raw: &[u8], seen: bool) -> Result<()> {
        let flags: &[&str] = if seen { &["\\Seen"] } else { &[] };
        self.client()?.append(path, raw, flags).await?;
        Ok(())
    }
}

#[async_trait]
impl MailTransport for ImapTransport {
    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            server_side_folders: true,
            server_side_flags: true,
            stable_ids: true, // UIDs, valid for as long as UIDVALIDITY holds
            // IDLE exists and the server supports it; the sync engine does not use it yet, so
            // claiming push here would make the UI promise something it does not deliver.
            push: false,
            partial_fetch: true,
            deferred_delete: false,
        }
    }

    async fn connect(&mut self) -> Result<()> {
        let client = ImapClient::connect(
            &self.config,
            &self.credentials.user,
            &self.credentials.password,
            self.credentials.allow_plaintext,
        )
        .await?;
        tracing::info!(
            host = %self.config.host,
            capabilities = client.capabilities().len(),
            "imap session established"
        );
        self.client = Some(client);
        Ok(())
    }

    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>> {
        let mailboxes = self.client()?.list_mailboxes().await?;
        Ok(mailboxes
            .into_iter()
            .filter(|m| m.selectable)
            .map(|m| RemoteFolder {
                kind: folder_kind(classify(&m)),
                id: FolderId(m.path.clone()),
                name: m.name,
            })
            .collect())
    }

    async fn list_messages(&mut self, folder: &FolderId) -> Result<Vec<RemoteMessage>> {
        let path = folder.0.clone();
        let validity = self.client()?.select(&path).await?;
        self.validities.retain(|(p, _)| p != &path);
        self.validities.push((path.clone(), validity));

        let messages = self.client()?.list_messages(&path).await?;
        Ok(messages
            .into_iter()
            .map(|m| RemoteMessage {
                id: RemoteId(m.uid.to_string()),
                size: m.size,
                flags: m.flags.iter().filter_map(|f| parse_flag(f)).collect(),
            })
            .collect())
    }

    async fn fetch_headers(&mut self, folder: &FolderId, id: &RemoteId) -> Result<Vec<u8>> {
        let uid = parse_uid(id)?;
        let path = folder.0.clone();
        Ok(self.client()?.fetch_headers(&path, uid).await?)
    }

    async fn fetch_message(
        &mut self,
        folder: &FolderId,
        id: &RemoteId,
        out: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<u64> {
        let uid = parse_uid(id)?;
        let path = folder.0.clone();
        Ok(self.client()?.fetch_message(&path, uid, out).await?)
    }

    async fn set_flags(&mut self, folder: &FolderId, id: &RemoteId, flags: &[Flag]) -> Result<()> {
        let uid = parse_uid(id)?;
        let path = folder.0.clone();
        let names: Vec<&str> = flags.iter().map(flag_name).collect();
        self.client()?.store_flags(&path, uid, &names, true).await?;
        Ok(())
    }

    async fn delete(&mut self, folder: &FolderId, id: &RemoteId) -> Result<()> {
        let uid = parse_uid(id)?;
        let path = folder.0.clone();
        self.client()?.delete_message(&path, uid).await?;
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        // IMAP applies each change as it is made; there is nothing staged to commit. LOGOUT is
        // simply the polite way to end the session.
        match self.client {
            Some(client) => Ok(client.logout().await?),
            None => Ok(()),
        }
    }
}

fn parse_uid(id: &RemoteId) -> Result<u32> {
    id.0.parse()
        .map_err(|_| Error::UnknownMessage(format!("{:?} is not an IMAP UID", id.0)))
}

fn folder_kind(kind: &str) -> FolderKind {
    match kind {
        "inbox" => FolderKind::Inbox,
        "sent" => FolderKind::Sent,
        "drafts" => FolderKind::Drafts,
        "deleted" => FolderKind::Deleted,
        "junk" => FolderKind::Junk,
        "archive" => FolderKind::Archive,
        _ => FolderKind::Other,
    }
}

fn flag_name(flag: &Flag) -> &'static str {
    match flag {
        Flag::Seen => "\\Seen",
        Flag::Answered => "\\Answered",
        Flag::Flagged => "\\Flagged",
        Flag::Draft => "\\Draft",
        Flag::Deleted => "\\Deleted",
    }
}

/// Map a server flag onto ours. Custom keywords are ignored rather than guessed at.
fn parse_flag(raw: &str) -> Option<Flag> {
    let lower = raw.to_lowercase();
    if lower.contains("seen") {
        Some(Flag::Seen)
    } else if lower.contains("answered") {
        Some(Flag::Answered)
    } else if lower.contains("flagged") {
        Some(Flag::Flagged)
    } else if lower.contains("draft") {
        Some(Flag::Draft)
    } else if lower.contains("deleted") {
        Some(Flag::Deleted)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> ImapTransport {
        ImapTransport::new(
            ImapConfig::implicit_tls("mail.example.com"),
            ImapCredentials {
                user: "u".into(),
                password: "hunter2".into(),
                allow_plaintext: false,
            },
        )
    }

    #[test]
    fn capabilities_describe_a_full_transport() {
        let caps = transport().capabilities();
        // The whole reason to offer IMAP for a shared mailbox.
        assert!(caps.server_side_flags);
        assert!(caps.server_side_folders);
        assert!(caps.stable_ids);
        assert!(caps.partial_fetch);
        // IMAP applies changes immediately; nothing is staged until a QUIT.
        assert!(!caps.deferred_delete);
        // The UI keys its "state is local only" warning off this, and IMAP must not trigger it.
        assert!(!caps.is_local_authoritative());
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
        let err = t.list_folders().await.unwrap_err();
        assert!(matches!(err, Error::NotConnected), "got {err:?}");
    }

    #[test]
    fn a_non_numeric_remote_id_is_rejected() {
        // POP3 UIDLs are opaque strings; IMAP UIDs are numbers. Mixing them up would act on the
        // wrong message, so it is an error rather than a silent zero.
        assert!(parse_uid(&RemoteId("GmlmZTAwMQ".into())).is_err());
        assert_eq!(parse_uid(&RemoteId("4471".into())).unwrap(), 4471);
    }

    #[test]
    fn server_flags_map_onto_ours() {
        assert_eq!(parse_flag("Seen"), Some(Flag::Seen));
        assert_eq!(parse_flag("\\\\Flagged"), Some(Flag::Flagged));
        assert_eq!(parse_flag("Answered"), Some(Flag::Answered));
        // A custom keyword is not something we can interpret, so it is dropped.
        assert_eq!(parse_flag("$assigned_sarah"), None);
    }

    #[test]
    fn folder_roles_map_onto_our_kinds() {
        assert_eq!(folder_kind("inbox"), FolderKind::Inbox);
        assert_eq!(folder_kind("sent"), FolderKind::Sent);
        assert_eq!(folder_kind("junk"), FolderKind::Junk);
        assert_eq!(folder_kind("something-else"), FolderKind::Other);
    }
}
