//! Row types shared by the store and everything above it.

use serde::{Deserialize, Serialize};

/// Transport security for a server connection. Mirrors `signet_net::pop3::Security` without making
/// `signet-store` depend on the network layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    Tls,
    Starttls,
    Plain,
}

impl Security {
    pub fn as_str(self) -> &'static str {
        match self {
            Security::Tls => "tls",
            Security::Starttls => "starttls",
            Security::Plain => "plain",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "tls" => Security::Tls,
            "plain" => Security::Plain,
            _ => Security::Starttls,
        }
    }

    /// Conventional POP3 port for this mode.
    pub fn default_pop_port(self) -> u16 {
        match self {
            Security::Tls => 995,
            _ => 110,
        }
    }

    /// Conventional SMTP submission port for this mode.
    pub fn default_smtp_port(self) -> u16 {
        match self {
            Security::Tls => 465,
            _ => 587,
        }
    }
}

/// How we talk to the incoming server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Download-and-forget. One folder, no flags, no push (PLAN.md §2).
    #[default]
    Pop3,
    /// Server-side folders and flags, shared across every client. What a shared mailbox wants.
    Imap,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Pop3 => "pop3",
            Protocol::Imap => "imap",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "imap" => Protocol::Imap,
            _ => Protocol::Pop3,
        }
    }

    /// Conventional incoming port for this protocol and security mode.
    pub fn default_port(self, security: Security) -> u16 {
        match (self, security) {
            (Protocol::Pop3, Security::Tls) => 995,
            (Protocol::Pop3, _) => 110,
            (Protocol::Imap, Security::Tls) => 993,
            (Protocol::Imap, _) => 143,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: i64,
    pub display_name: String,
    pub email: String,
    pub pop_host: String,
    pub pop_port: u16,
    pub pop_security: Security,
    pub pop_user: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub smtp_user: String,
    pub credential_handle: String,
    pub leave_on_server: bool,
    pub delete_after_days: Option<i64>,
    pub poll_minutes: i64,
    pub signature: String,
    pub is_enabled: bool,
    pub last_sync_utc: Option<i64>,
    pub junk_filter_enabled: bool,
    /// Junk score at or above which a message is filed. Higher means more permissive.
    pub junk_threshold: f64,
    pub protocol: Protocol,
}

/// The account settings a user can change after creation.
///
/// Server details are deliberately absent: changing them is closer to adding a new account than
/// editing one, and doing it in place would strand the stored credential and the UIDL history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSettings {
    pub display_name: String,
    pub signature: String,
    pub poll_minutes: i64,
    pub leave_on_server: bool,
    pub delete_after_days: Option<i64>,
    pub junk_filter_enabled: bool,
    pub junk_threshold: f64,
    pub is_enabled: bool,
}

/// Everything needed to create an account except the password, which goes to the keystore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAccount {
    pub display_name: String,
    pub email: String,
    pub pop_host: String,
    pub pop_port: u16,
    pub pop_security: Security,
    pub pop_user: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub smtp_user: String,
    pub leave_on_server: bool,
    pub delete_after_days: Option<i64>,
    pub poll_minutes: i64,
    pub signature: String,
    #[serde(default)]
    pub protocol: Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderKind {
    Inbox,
    Sent,
    Drafts,
    Deleted,
    Junk,
    Archive,
    Custom,
}

impl FolderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FolderKind::Inbox => "inbox",
            FolderKind::Sent => "sent",
            FolderKind::Drafts => "drafts",
            FolderKind::Deleted => "deleted",
            FolderKind::Junk => "junk",
            FolderKind::Archive => "archive",
            FolderKind::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "inbox" => FolderKind::Inbox,
            "sent" => FolderKind::Sent,
            "drafts" => FolderKind::Drafts,
            "deleted" => FolderKind::Deleted,
            "junk" => FolderKind::Junk,
            "archive" => FolderKind::Archive,
            _ => FolderKind::Custom,
        }
    }

    /// The folders every new account starts with. POP3 gives us none of these — we invent them.
    pub fn defaults() -> [(FolderKind, &'static str); 6] {
        [
            (FolderKind::Inbox, "Inbox"),
            (FolderKind::Drafts, "Drafts"),
            (FolderKind::Sent, "Sent"),
            (FolderKind::Deleted, "Deleted Items"),
            (FolderKind::Junk, "Junk"),
            (FolderKind::Archive, "Archive"),
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub account_id: i64,
    pub kind: FolderKind,
    pub name: String,
    pub sort_order: i64,
    pub total: i64,
    pub unread: i64,
}

/// What the message list renders. Deliberately excludes bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: i64,
    pub account_id: i64,
    pub folder_id: i64,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub date_utc: i64,
    pub is_read: bool,
    pub is_flagged: bool,
    pub is_answered: bool,
    pub has_attachments: bool,
    pub size_bytes: i64,
    pub preview_text: String,
    /// 0 low, 1 normal, 2 high.
    pub priority: i64,
    pub work_status: String,
    /// Denormalised so the list can draw the assignee chip without a query per row.
    pub assignee_name: Option<String>,
    pub assignee_color: Option<String>,
    pub assignee_initials: Option<String>,
    /// Due time of the open reminder, if any. Drives the overdue highlight.
    pub reminder_due_utc: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipient {
    pub kind: String,
    pub name: String,
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: i64,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub content_id: Option<String>,
    pub is_inline: bool,
}

/// A message opened in the reading pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDetail {
    pub id: i64,
    pub account_id: i64,
    pub folder_id: i64,
    pub rfc822_message_id: Option<String>,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub date_utc: i64,
    pub is_read: bool,
    pub is_flagged: bool,
    pub is_answered: bool,
    /// Sanitized HTML with remote resources already stripped, or `None` for text-only mail.
    pub body_html: Option<String>,
    pub body_text: String,
    /// True when sanitization removed remote references, which is what drives the
    /// "this message tried to load remote content" banner.
    pub has_remote_content: bool,
    pub recipients: Vec<Recipient>,
    pub attachments: Vec<Attachment>,
    pub size_bytes: i64,
}

/// Filter for the message list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageQuery {
    /// `None` means every account — the unified inbox.
    pub account_id: Option<i64>,
    pub folder_id: Option<i64>,
    /// When `folder_id` is absent, match folders of this kind across accounts.
    pub folder_kind: Option<FolderKind>,
    pub unread_only: bool,
    pub flagged_only: bool,
    /// Only mail assigned to this member.
    pub assignee_id: Option<i64>,
    /// Only mail nobody has picked up.
    pub unassigned_only: bool,
    /// Only high priority.
    pub high_priority_only: bool,
    /// Only messages with an open reminder.
    pub has_reminder_only: bool,
    /// FTS5 query. Empty means no text filter.
    pub search: String,
    pub limit: i64,
    pub offset: i64,
}
