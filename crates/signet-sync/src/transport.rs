//! The transport abstraction (ADR 0001 / D3).
//!
//! **This trait is modelled on the superset — folders, flags, server-side state — and POP3
//! implements a degenerate case of it.** That direction is the whole point. Modelling it on POP3
//! and widening it later would leak "there is exactly one folder and no flags" into every caller,
//! which is precisely the refactor this trait exists to avoid.
//!
//! v1 ships only [`crate::pop3_transport::Pop3Transport`]. An IMAP implementation would slot in
//! beside it without the sync engine noticing.

use std::fmt::Debug;

use async_trait::async_trait;
use tokio::io::AsyncWrite;

use crate::{Error, Result};

/// Identifies a folder on the server. POP3 has exactly one, named `INBOX`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FolderId(pub String);

impl FolderId {
    pub fn inbox() -> Self {
        Self("INBOX".to_string())
    }
}

/// A server-side message identifier, stable across sessions.
///
/// For POP3 this is the `UIDL` string — deliberately *not* the message number, which servers
/// renumber between sessions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    pub id: FolderId,
    pub name: String,
    pub kind: FolderKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    Inbox,
    Sent,
    Drafts,
    Deleted,
    Junk,
    Archive,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMessage {
    pub id: RemoteId,
    pub size: u64,
    /// Server-held flags. Always empty for POP3 — the server has no idea what we have read.
    pub flags: Vec<Flag>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Seen,
    Answered,
    Flagged,
    Draft,
    Deleted,
}

/// What a given transport can actually do.
///
/// The sync engine branches on these rather than on the transport's concrete type, so adding IMAP
/// does not mean auditing every call site for POP3 assumptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportCapabilities {
    /// Server has a folder hierarchy we can enumerate and move messages between.
    pub server_side_folders: bool,
    /// Server stores read/answered/flagged state, so it can sync across devices.
    pub server_side_flags: bool,
    /// Message identifiers survive across sessions (POP3 `UIDL`, IMAP `UID`).
    pub stable_ids: bool,
    /// Server can notify us of new mail without polling (IMAP `IDLE`).
    pub push: bool,
    /// Headers can be fetched without the body (POP3 `TOP`, IMAP `BODY.PEEK[HEADER]`).
    pub partial_fetch: bool,
    /// Deletions are staged and committed at the end of the session rather than immediately.
    pub deferred_delete: bool,
}

impl TransportCapabilities {
    /// True when local state is the only record of reads, folders and flags — which decides
    /// whether the UI should warn about multi-device divergence (PLAN.md §2).
    pub fn is_local_authoritative(&self) -> bool {
        !self.server_side_flags || !self.server_side_folders
    }
}

/// A mail source. One connection, one session.
///
/// Implementations are **not** expected to be reconnect-transparent: the sync engine owns the
/// retry policy, because only it knows whether a partially-ingested batch is safe to repeat.
#[async_trait]
pub trait MailTransport: Debug + Send {
    fn capabilities(&self) -> TransportCapabilities;

    /// Open the session and authenticate.
    async fn connect(&mut self) -> Result<()>;

    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>>;

    async fn list_messages(&mut self, folder: &FolderId) -> Result<Vec<RemoteMessage>>;

    /// Fetch headers only, for the filter-before-download pass. Requires
    /// [`TransportCapabilities::partial_fetch`].
    async fn fetch_headers(&mut self, folder: &FolderId, id: &RemoteId) -> Result<Vec<u8>>;

    /// Stream a full message into `out`. Never buffers the whole message in memory.
    async fn fetch_message(
        &mut self,
        folder: &FolderId,
        id: &RemoteId,
        out: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<u64>;

    /// Set server-side flags. A no-op for transports without
    /// [`TransportCapabilities::server_side_flags`] — callers do not need to check first.
    async fn set_flags(&mut self, folder: &FolderId, id: &RemoteId, flags: &[Flag]) -> Result<()>;

    /// Mark for deletion. With [`TransportCapabilities::deferred_delete`] this does not take
    /// effect until [`MailTransport::commit`].
    async fn delete(&mut self, folder: &FolderId, id: &RemoteId) -> Result<()>;

    /// Commit staged mutations and end the session cleanly.
    ///
    /// Consuming the box is deliberate: for POP3 this is `QUIT`, and **dropping the transport
    /// without calling it is the only way to guarantee no deletion is committed** — which is what
    /// every error path wants.
    async fn commit(self: Box<Self>) -> Result<()>;
}

/// Returned by transports that lack a capability the caller demanded anyway.
pub fn unsupported(what: &'static str) -> Error {
    Error::Unsupported(what)
}
