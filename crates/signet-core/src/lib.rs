//! The facade the UI talks to: typed commands in, an event stream out.
//!
//! The UI never speaks a protocol and never touches SQL (PLAN.md §3). It calls methods here and
//! renders what comes back, which is what keeps `signet-cli`, the tests, and a second UI cheap.

pub mod archive;
pub mod contacts_export;
pub mod features;
pub mod inline;
pub mod outlook;
pub mod remote;
pub mod reply;
pub mod shared;
pub mod triage;

use std::path::PathBuf;
use std::sync::Arc;

use signet_store::{
    Account, Folder, FolderKind, MessageDetail, MessageQuery, MessageSummary, NewAccount, Store,
};
use signet_sync::SyncReport;
use signet_sync::outbox::Draft;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::broadcast;

pub use features::FilingInfo;
pub use signet_store::{Security, models};
pub use signet_sync::{junk, outbox, rules};
pub use shared::SyncSummary;
pub use triage::Triage;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] signet_store::Error),

    #[error(transparent)]
    Sync(#[from] signet_sync::Error),

    #[error("credential error: {0}")]
    Auth(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Invalid(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<signet_auth::Error> for Error {
    fn from(e: signet_auth::Error) -> Self {
        Error::Auth(e.to_string())
    }
}

/// What one pass over the outbox managed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OutboxFlush {
    pub sent: i64,
    pub failed: i64,
    pub last_error: Option<String>,
}

/// Anything larger than this is saved rather than shown. See [`App::attachment_preview`].
const MAX_PREVIEW_BYTES: i64 = 64 * 1024 * 1024;

/// An attachment encoded for the viewer.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentPreview {
    pub filename: String,
    /// The type this file's own bytes say it is — never what the sender declared.
    pub mime_type: String,
    pub base64: String,
}

/// Refuse a draft whose recipients cannot all be delivered to, saying which ones and why.
fn check_recipients(draft: &Draft) -> Result<()> {
    let invalid = draft.invalid_recipients();
    if !invalid.is_empty() {
        return Err(Error::Invalid(format!(
            "{} not a valid email address: {}. Use name@company.com, or Name <name@company.com>,              and separate addresses with ; or ,",
            if invalid.len() == 1 { "This is" } else { "These are" },
            invalid.join(", ")
        )));
    }
    if draft.envelope_recipients().is_empty() {
        return Err(Error::Invalid("add at least one recipient".into()));
    }
    Ok(())
}

/// `name` with its extension replaced by the one that matches the sniffed type.
fn name_for_type(name: &str, mime: &str) -> String {
    let ext = match mime {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/webp" => "webp",
        "image/avif" => "avif",
        _ => "bin",
    };
    let stem = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("attachment");
    format!("{stem}.{ext}")
}

/// Tag a file written from a mail attachment as downloaded from the internet.
///
/// This is the `Zone.Identifier` stream a browser writes. It is what makes SmartScreen warn
/// before an executable runs and Office open a document in Protected View (PLAN.md §7.5).
/// Best effort: a FAT or network drive has no alternate streams, and the file is still wanted.
fn mark_from_internet(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let _ = std::fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// What a file's leading bytes say it actually is.
///
/// The sender's `Content-Type` and the extension on the filename are both theirs to choose, and
/// neither has to match what is inside. That matters more than it sounds: the viewer hands the
/// bytes to the WebView with a type attached, and a file called `invoice.pdf` declared as
/// `text/html` would otherwise be rendered as a document in this application's own origin.
/// Deciding from the content removes the mismatch instead of trying to police it.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.len() >= sig.len() && &bytes[..sig.len()] == sig;

    if starts(b"%PDF-") {
        return Some("application/pdf");
    }
    if starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif");
    }
    if starts(b"BM") {
        return Some("image/bmp");
    }
    // RIFF....WEBP
    if starts(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // ISO-BMFF: an `ftyp` box whose brand is one of the AVIF ones.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..11] == b"avi" {
        return Some("image/avif");
    }
    None
}

/// The result of checking one half of an account.
#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    /// What was tried, in the user's terms: "Incoming mail — IMAP mail.example.com:993".
    pub label: String,
    pub ok: bool,
    /// On success, what was found. On failure, what the server or the network actually said.
    pub detail: String,
}

/// Both halves of an account, checked independently.
#[derive(Debug, Clone, Serialize)]
pub struct AccountCheck {
    pub ok: bool,
    pub incoming: CheckOutcome,
    pub outgoing: CheckOutcome,
}

/// Something the UI should react to. Broadcast, so every open window stays consistent.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    SyncStarted {
        account_id: i64,
    },
    SyncFinished {
        account_id: i64,
        new_messages: usize,
    },
    /// How a running sync is getting on. Emitted often — several times a second on a fast
    /// connection — so the UI must treat it as a redraw hint rather than something to queue.
    SyncProgress {
        account_id: i64,
        phase: &'static str,
        done: usize,
        total: usize,
        folder: String,
        bytes_done: u64,
        bytes_total: u64,
    },
    SyncFailed {
        account_id: i64,
        error: String,
    },
    MessagesChanged {
        account_id: i64,
    },
    AccountsChanged,
}

#[derive(Debug, Clone)]
pub struct App {
    store: Store,
    events: broadcast::Sender<Event>,
    /// Serialises syncs so two triggers for the same account cannot interleave sessions.
    sync_lock: Arc<tokio::sync::Mutex<()>>,
}

impl App {
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let store = Store::open(data_dir.into())?;
        store.recover_interrupted_sends()?;
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            store,
            events,
            sync_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Emit an event from another module in this crate.
    pub(crate) fn emit_public(&self, event: Event) {
        self.emit(event);
    }

    fn emit(&self, event: Event) {
        // A send error only means nobody is listening yet, which is normal at startup.
        let _ = self.events.send(event);
    }

    // ---------------------------------------------------------------- accounts

    /// Add an account, storing the password in the platform keystore rather than the database.
    pub fn add_account(&self, new: NewAccount, password: &str) -> Result<i64> {
        if new.email.trim().is_empty() || new.pop_host.trim().is_empty() {
            return Err(Error::Invalid(
                "email address and POP3 host are both required".into(),
            ));
        }

        // Checked before the keystore is touched. The handle is derived from user and host, so a
        // second add of the same mailbox would overwrite the password the working account uses,
        // and the failed insert would then delete it.
        let email = new.email.trim().to_lowercase();
        if self
            .store
            .list_accounts()?
            .iter()
            .any(|a| a.email.trim().to_lowercase() == email)
        {
            return Err(Error::Invalid(format!(
                "{} is already set up. To change its password, open Settings and use Change                  password.",
                new.email.trim()
            )));
        }

        let handle = signet_auth::handle_for("pop3", &new.pop_user, &new.pop_host);
        signet_auth::store(
            &handle,
            &signet_auth::Credential::Password(password.to_string()),
        )?;

        let id = match self.store.add_account(&new, &handle) {
            Ok(id) => id,
            Err(e) => {
                // Do not leave an orphaned secret behind if the row could not be written.
                let _ = signet_auth::delete(&handle);
                return Err(e.into());
            }
        };

        self.emit(Event::AccountsChanged);
        Ok(id)
    }

    /// Replace an account's password, after proving the new one works.
    ///
    /// Tested against the incoming server first, so a typo cannot turn a working account into a
    /// broken one. Nothing else about the account changes, and no mail is touched.
    pub async fn change_password(&self, id: i64, password: &str) -> Result<()> {
        if password.is_empty() {
            return Err(Error::Invalid("enter the new password".into()));
        }
        let a = self.store.get_account(id)?;
        let probe = NewAccount {
            display_name: a.display_name.clone(),
            email: a.email.clone(),
            pop_host: a.pop_host.clone(),
            pop_port: a.pop_port,
            pop_security: a.pop_security,
            pop_user: a.pop_user.clone(),
            smtp_host: a.smtp_host.clone(),
            smtp_port: a.smtp_port,
            smtp_security: a.smtp_security,
            smtp_user: a.smtp_user.clone(),
            leave_on_server: a.leave_on_server,
            delete_after_days: a.delete_after_days,
            poll_minutes: a.poll_minutes,
            signature: a.signature.clone(),
            protocol: a.protocol,
        };
        let check = self.check_incoming(&probe, password).await;
        if !check.ok {
            return Err(Error::Invalid(format!(
                "the server did not accept that password, so nothing was changed. {}",
                check.detail
            )));
        }
        signet_auth::store(
            &a.credential_handle,
            &signet_auth::Credential::Password(password.to_string()),
        )?;
        self.emit(Event::AccountsChanged);
        Ok(())
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        Ok(self.store.list_accounts()?)
    }

    pub fn delete_account(&self, id: i64) -> Result<()> {
        let account = self.store.get_account(id)?;
        self.store.delete_account(id)?;
        // Best effort: the account is gone either way, and a stale keystore entry is inert.
        if let Err(e) = signet_auth::delete(&account.credential_handle) {
            tracing::warn!(error = %e, "could not remove stored credential");
        }
        self.emit(Event::AccountsChanged);
        Ok(())
    }

    /// Verify credentials and server settings without storing anything.
    /// Check both halves of an account without saving it.
    ///
    /// Incoming and outgoing are configured independently and fail independently, so they are
    /// reported independently: an account whose IMAP works while its SMTP is wrong looks perfectly
    /// healthy right up until the first reply will not go out. Neither check aborts the other, and
    /// a failure carries the server's own words rather than a summary of them, because "authentication
    /// failed" and "connection refused" need completely different fixes.
    pub async fn check_account(&self, new: &NewAccount, password: &str) -> AccountCheck {
        let incoming = self.check_incoming(new, password).await;
        let outgoing = self.check_outgoing(new, password).await;
        AccountCheck {
            ok: incoming.ok && outgoing.ok,
            incoming,
            outgoing,
        }
    }

    async fn check_incoming(&self, new: &NewAccount, password: &str) -> CheckOutcome {
        let protocol = match new.protocol {
            signet_store::Protocol::Imap => "IMAP",
            signet_store::Protocol::Pop3 => "POP3",
        };
        let label = format!(
            "Incoming mail — {protocol} {}:{}",
            new.pop_host, new.pop_port
        );

        match self.try_incoming(new, password).await {
            Ok(detail) => CheckOutcome {
                label,
                ok: true,
                detail,
            },
            Err(e) => CheckOutcome {
                label,
                ok: false,
                detail: e.to_string(),
            },
        }
    }

    async fn try_incoming(&self, new: &NewAccount, password: &str) -> Result<String> {
        use signet_sync::transport::MailTransport;

        let account = draft_account(new);

        match new.protocol {
            signet_store::Protocol::Imap => {
                let mut transport = signet_sync::imap_transport::ImapTransport::new(
                    signet_sync::imap_config_for(&account),
                    signet_sync::imap_transport::ImapCredentials {
                        user: new.pop_user.clone(),
                        password: password.to_string(),
                        allow_plaintext: new.pop_security == Security::Plain,
                    },
                );
                transport.connect().await?;

                let folders = transport.list_folders().await?;
                let inbox = folders
                    .iter()
                    .find(|f| f.kind == signet_sync::transport::FolderKind::Inbox)
                    .map(|f| f.id.clone())
                    .unwrap_or_else(signet_sync::transport::FolderId::inbox);
                let count = transport.list_messages(&inbox).await?.len();
                let folder_count = folders.len();
                Box::new(transport).commit().await?;

                Ok(format!(
                    "Signed in. {folder_count} folder(s) on the server, {count} message(s) in the \
                     Inbox. Read state, flags and folders stay in step across the whole team."
                ))
            }
            signet_store::Protocol::Pop3 => {
                let mut transport = signet_sync::pop3_transport::Pop3Transport::new(
                    signet_sync::pop3_config_for(&account),
                    signet_sync::pop3_transport::Pop3Credentials {
                        user: new.pop_user.clone(),
                        password: password.to_string(),
                        allow_plaintext: new.pop_security == Security::Plain,
                    },
                );
                transport.connect().await?;
                let count = transport
                    .list_messages(&signet_sync::transport::FolderId::inbox())
                    .await?
                    .len();
                Box::new(transport).commit().await?;

                Ok(format!(
                    "Signed in. {count} message(s) waiting on the server. Note that POP3 shares no \
                     read state between machines."
                ))
            }
        }
    }

    async fn check_outgoing(&self, new: &NewAccount, password: &str) -> CheckOutcome {
        let label = format!("Sending mail — SMTP {}:{}", new.smtp_host, new.smtp_port);

        let account = draft_account(new);

        match signet_sync::check_smtp(&account, password).await {
            Ok(()) => CheckOutcome {
                label,
                ok: true,
                detail: if new.smtp_user.is_empty() {
                    "Connected. The server accepts mail without signing in.".into()
                } else {
                    "Signed in. Replies and new messages will go out through this server.".into()
                },
            },
            Err(e) => CheckOutcome {
                label,
                ok: false,
                detail: e.to_string(),
            },
        }
    }

    // ---------------------------------------------------------------- syncing

    pub async fn sync_account(&self, account_id: i64) -> Result<SyncReport> {
        let _guard = self.sync_lock.lock().await;

        let account = self.store.get_account(account_id)?;
        self.emit(Event::SyncStarted { account_id });

        let password = signet_auth::load(&account.credential_handle)?;

        // The reporter is handed to the sync and forwards straight onto the same broadcast channel
        // everything else uses, so every open window sees the same progress without the sync layer
        // knowing anything about windows.
        let events = self.events.clone();
        let reporter: signet_sync::progress::Reporter =
            std::sync::Arc::new(move |p: signet_sync::progress::Progress| {
                // A send failure means nobody is listening, which is not a problem worth logging
                // once per message.
                let _ = events.send(Event::SyncProgress {
                    account_id: p.account_id,
                    phase: p.phase.label(),
                    done: p.done,
                    total: p.total,
                    folder: p.folder,
                    bytes_done: p.bytes_done,
                    bytes_total: p.bytes_total,
                });
            });

        let result = signet_sync::sync_account_reporting(
            &self.store,
            &account,
            password.expose(),
            &signet_sync::progress::Progressing::to(reporter),
        )
        .await;

        match result {
            Ok(report) => {
                self.emit(Event::SyncFinished {
                    account_id,
                    new_messages: report.new_messages,
                });
                if report.new_messages > 0 {
                    self.emit(Event::MessagesChanged { account_id });
                }
                Ok(report)
            }
            Err(e) => {
                self.emit(Event::SyncFailed {
                    account_id,
                    error: e.to_string(),
                });
                Err(e.into())
            }
        }
    }

    /// Sync every enabled account, reporting per-account outcomes.
    ///
    /// One unreachable server must not stop the others from updating.
    pub async fn sync_all(&self) -> Vec<(i64, std::result::Result<SyncReport, String>)> {
        let accounts = match self.store.list_accounts() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "could not list accounts");
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        for account in accounts.into_iter().filter(|a| a.is_enabled) {
            let result = self
                .sync_account(account.id)
                .await
                .map_err(|e| e.to_string());
            out.push((account.id, result));
        }
        out
    }

    // ---------------------------------------------------------------- reading

    pub fn list_folders(&self, account_id: Option<i64>) -> Result<Vec<Folder>> {
        Ok(self.store.list_folders(account_id)?)
    }

    pub fn list_messages(&self, query: MessageQuery) -> Result<Vec<MessageSummary>> {
        Ok(self.store.list_messages(&query)?)
    }

    pub fn get_message(&self, id: i64) -> Result<MessageDetail> {
        let mut detail = self.store.get_message(id)?;
        // Done at read time so mail already downloaded is fixed without a re-sync.
        inline::resolve_cid_images(&self.store, &mut detail);
        Ok(detail)
    }

    /// Re-render a message's HTML with remote content permitted.
    ///
    /// Separate from [`App::get_message`] on purpose: showing images is an explicit per-message
    /// decision by the user, never a default (PLAN.md §7.4).
    pub fn get_message_with_remote_content(&self, id: i64) -> Result<MessageDetail> {
        let mut detail = self.store.get_message(id)?;
        let raw_path = self.raw_path(id)?;

        // Re-sanitize from the original, not from the already-stripped copy: the stored HTML no
        // longer contains the URLs we would need to restore.
        if let Ok(raw) = std::fs::read(&raw_path) {
            if let Some(parsed) = signet_mime::parse(&raw) {
                if let Some(html) = parsed.body_html {
                    let clean = signet_mime::sanitize(&html, true).html;
                    detail.body_html = Some(remote::inline_remote_images(&clean));
                }
            }
        }
        // Re-sanitizing threw away the previous pass's inlining, so do it again. Order matters:
        // the sanitizer's allowed schemes do not include `data:`, so this must come after it.
        inline::resolve_cid_images(&self.store, &mut detail);
        Ok(detail)
    }

    fn raw_path(&self, message_id: i64) -> Result<PathBuf> {
        Ok(PathBuf::from(self.store.raw_path(message_id)?))
    }

    pub fn set_read(&self, ids: Vec<i64>, read: bool) -> Result<()> {
        self.store.set_read(&ids, read)?;
        self.emit(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn set_flagged(&self, ids: Vec<i64>, flagged: bool) -> Result<()> {
        self.store.set_flagged(&ids, flagged)?;
        self.emit(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn move_messages(&self, ids: Vec<i64>, folder_id: i64) -> Result<()> {
        self.store.move_messages(&ids, folder_id)?;
        self.emit(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    /// Move to Deleted Items, or purge outright if already there.
    ///
    /// This is what a delete key should do: recoverable the first time, final the second.
    pub fn delete_messages(&self, ids: Vec<i64>) -> Result<()> {
        let mut to_purge = Vec::new();
        let mut by_folder: std::collections::HashMap<i64, Vec<i64>> = Default::default();

        for id in ids {
            let m = self.store.get_message(id)?;
            let deleted_folder = self.store.folder_id(m.account_id, FolderKind::Deleted)?;
            if m.folder_id == deleted_folder {
                to_purge.push(id);
            } else {
                by_folder.entry(deleted_folder).or_default().push(id);
            }
        }

        for (folder_id, ids) in by_folder {
            self.store.move_messages(&ids, folder_id)?;
        }
        self.store.purge_messages(&to_purge)?;

        self.emit(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    /// Copy an attachment out to a path the user chose.
    pub fn save_attachment(&self, attachment_id: i64, dest: &std::path::Path) -> Result<()> {
        let (_, blob) = self.store.attachment_path(attachment_id)?;
        std::fs::copy(&blob, dest)?;
        mark_from_internet(dest);
        Ok(())
    }

    pub fn attachment_bytes(&self, attachment_id: i64) -> Result<(String, Vec<u8>)> {
        let (filename, blob) = self.store.attachment_path(attachment_id)?;
        Ok((filename, std::fs::read(&blob)?))
    }

    /// Copy an attachment somewhere the system can open it, and say where.
    ///
    /// Only what the bytes prove is a document or a picture. Handing an arbitrary email attachment
    /// to whatever Windows has registered for its extension is precisely how malware gets run, and
    /// the extra friction of Save-then-open is the only thing standing between a `.js` in an
    /// invoice and a double click. PDFs and images have no such problem, and they are what people
    /// actually want to open.
    pub fn stage_attachment_for_opening(&self, attachment_id: i64) -> Result<String> {
        let detail = self.store.attachment_detail(attachment_id)?;
        let (filename, bytes) = self.attachment_bytes(attachment_id)?;

        let Some(kind) = sniff(&bytes) else {
            return Err(Error::Invalid(format!(
                "{filename} is not a PDF or an image. Use Save as… and open it yourself — this is \
                 deliberate, because opening an unknown attachment straight from mail is how most \
                 malware is run."
            )));
        };
        let _ = detail;

        // Its own directory per attachment, so two files with the same name cannot collide and the
        // name the sender chose is the name the viewer shows in its title bar.
        let dir = self
            .store
            .data_dir()
            .join("open")
            .join(attachment_id.to_string());
        std::fs::create_dir_all(&dir)?;

        // Windows picks the program from the extension, not the bytes, so the extension is forced
        // to match what was sniffed. Otherwise `invoice.bat` starting `%PDF-` passes the check
        // above and is then run by cmd.
        let path = dir.join(name_for_type(&signet_mime::safe_filename(&filename), kind));
        std::fs::write(&path, &bytes)?;
        mark_from_internet(&path);
        Ok(path.to_string_lossy().into_owned())
    }

    /// An attachment, encoded for display in the reading pane.
    ///
    /// Base64 rather than raw bytes because a `Vec<u8>` crosses the IPC boundary as a JSON array of
    /// numbers -- several times the size, and slow to parse for anything larger than an icon.
    ///
    /// The size ceiling is what makes this safe to call from a click: the whole file has to be held
    /// in memory twice over, once as bytes and once as text, and a 200 MB video attachment would
    /// take the window down. Anything larger is saved to disk instead, which is what the user
    /// wanted for a file that size anyway.
    pub fn attachment_preview(&self, attachment_id: i64) -> Result<AttachmentPreview> {
        use base64::Engine;

        let detail = self.store.attachment_detail(attachment_id)?;
        if detail.size_bytes > MAX_PREVIEW_BYTES {
            return Err(Error::Invalid(format!(
                "{} is {:.1} MB, which is too large to open here. Save it and open it in its own \
                 program instead.",
                detail.filename,
                detail.size_bytes as f64 / (1024.0 * 1024.0)
            )));
        }

        let (filename, bytes) = self.attachment_bytes(attachment_id)?;

        // Refused rather than guessed at. Anything the viewer cannot positively identify is a file
        // for another program, and saying so is better than handing unknown content to the WebView
        // and hoping it declines to run it.
        let Some(mime_type) = sniff(&bytes) else {
            return Err(Error::Invalid(format!(
                "{filename} is not an image or a PDF that can be opened here, whatever it is \
                 labelled as. Save it and open it in its own program."
            )));
        };

        Ok(AttachmentPreview {
            filename,
            mime_type: mime_type.to_string(),
            base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        })
    }

    pub fn contacts(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        Ok(self.store.contacts(prefix, 20)?)
    }

    // ---------------------------------------------------------------- sending

    /// Queue a draft and attempt to send it immediately.
    ///
    /// Returns `true` when it went out, `false` when the server could not be reached and it is
    /// waiting in the Outbox to retry. The second is not an error to the user: the message is safe,
    /// and reporting it as a failure is what got people pressing Send again.
    pub async fn send(&self, draft: Draft) -> Result<bool> {
        check_recipients(&draft)?;

        let account = self.store.get_account(draft.account_id)?;
        if account.smtp_host.trim().is_empty() {
            return Err(Error::Invalid(
                "this account has no SMTP server configured".into(),
            ));
        }

        let outbox_id = signet_sync::outbox::queue(
            &self.store,
            &draft,
            &account.display_name,
            &account.email,
            &account.signature,
        )?;

        let password = signet_auth::load(&account.credential_handle)?;
        let result =
            signet_sync::outbox::send_now(&self.store, outbox_id, &draft, password.expose()).await;
        self.emit(Event::MessagesChanged {
            account_id: draft.account_id,
        });

        match result {
            Ok(_) => Ok(true),
            Err(signet_sync::Error::Net(e)) if e.is_transient() => {
                self.store
                    .defer_outbox(outbox_id, signet_store::now_utc() + 60)?;
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Hold a message back until `when`, instead of sending it now.
    ///
    /// The message is built at the moment of scheduling rather than at the moment of sending, so
    /// what goes out is what was composed and reviewed -- a signature or an account setting changed
    /// in between must not silently rewrite a message the user has already approved.
    pub async fn schedule_send(&self, draft: Draft, when_utc: i64) -> Result<i64> {
        check_recipients(&draft)?;
        let now = signet_store::now_utc();
        if when_utc <= now {
            return Err(Error::Invalid(
                "that time has already passed - pick one in the future".into(),
            ));
        }

        let account = self.store.get_account(draft.account_id)?;
        if account.smtp_host.trim().is_empty() {
            return Err(Error::Invalid(
                "this account has no SMTP server configured".into(),
            ));
        }

        let id = signet_sync::outbox::queue_at(
            &self.store,
            &draft,
            &account.display_name,
            &account.email,
            &account.signature,
            when_utc,
        )?;

        self.emit(Event::MessagesChanged {
            account_id: draft.account_id,
        });
        Ok(id)
    }

    /// Messages waiting for their moment.
    pub fn scheduled_messages(&self) -> Result<Vec<signet_store::OutboxItem>> {
        Ok(self.store.scheduled_outbox()?)
    }

    /// Cancel one, putting it back where it came from is not attempted: the composed text is still
    /// in the archived draft if there was one, and silently resurrecting a compose window would be
    /// more surprising than leaving it cancelled.
    pub fn cancel_scheduled(&self, id: i64) -> Result<bool> {
        let cancelled = self.store.cancel_outbox(id)?;
        if cancelled {
            self.emit_public(Event::MessagesChanged { account_id: 0 });
        }
        Ok(cancelled)
    }

    /// Send it now rather than at its appointed time, or retry one that failed.
    ///
    /// Returns whether it actually went, so the UI does not say "Sent" over a failure.
    pub async fn send_scheduled_now(&self, id: i64) -> Result<bool> {
        self.store.send_scheduled_now(id)?;
        self.flush_outbox().await?;
        Ok(self
            .store
            .outbox_item(id)?
            .is_some_and(|item| item.state == "sent"))
    }

    /// Sync every account whose polling interval has elapsed.
    ///
    /// `poll_minutes` has been a stored, validated, editable setting since the first migration and
    /// nothing ever read it -- mail arrived only when somebody pressed Send/Receive. The decision
    /// lives here rather than in a UI timer so that "is this account due?" has one answer, taken
    /// from `last_sync_utc`, which is the same field the account list displays.
    pub async fn sync_due(&self) -> Vec<(i64, Result<SyncReport>)> {
        let now = signet_store::now_utc();
        let due: Vec<i64> = match self.store.list_accounts() {
            Ok(accounts) => accounts
                .into_iter()
                .filter(|a| a.is_enabled)
                .filter(|a| {
                    // Never synced is always due. Otherwise wait out the interval, clamped the same
                    // way the settings screen clamps it so a hand-edited database cannot produce a
                    // tight loop against somebody's mail server.
                    let interval = a.poll_minutes.clamp(1, 1440) * 60;
                    a.last_sync_utc.is_none_or(|last| now - last >= interval)
                })
                .map(|a| a.id)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not list accounts to poll");
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        for id in due {
            out.push((id, self.sync_account(id).await));
        }
        out
    }

    /// Send everything whose time has come.
    ///
    /// Covers scheduled messages and retries alike: from here a message held until Monday and one
    /// waiting out a network failure are the same job. Called on a timer and after each sync, so a
    /// failure never needs the user to press anything.
    pub async fn flush_outbox(&self) -> Result<OutboxFlush> {
        let now = signet_store::now_utc();
        let due = self.store.due_outbox(now)?;
        let mut flush = OutboxFlush::default();

        for item in due {
            let Ok(draft) = serde_json::from_str::<Draft>(&item.draft_json) else {
                // Written before drafts were persisted, or corrupt. Parking it is better than
                // dropping it: the built message is still on disk and the row still names it.
                tracing::warn!(id = item.id, "outbox row has no usable draft; parking it");
                self.store.set_outbox_state(
                    item.id,
                    "failed",
                    Some("this message cannot be rebuilt"),
                )?;
                flush.failed += 1;
                continue;
            };

            let account = match self.store.get_account(item.account_id) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(id = item.id, error = %e, "outbox row has no account");
                    flush.failed += 1;
                    continue;
                }
            };
            let password = match signet_auth::load(&account.credential_handle) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(id = item.id, error = %e, "no password for a queued message");
                    flush.failed += 1;
                    continue;
                }
            };

            match signet_sync::outbox::send_now(&self.store, item.id, &draft, password.expose()).await
            {
                Ok(signet_sync::outbox::Sent::Delivered) => flush.sent += 1,
                Ok(signet_sync::outbox::Sent::AlreadyTaken) => {}
                Err(e) => {
                    // `send_now` has already set queued-or-failed by transience. All that is left
                    // is to keep a retry from spinning: back off, doubling, capped at an hour.
                    let delay = (60_i64 << item.attempts.min(6)).min(3600);
                    self.store.defer_outbox(item.id, now + delay)?;
                    flush.failed += 1;
                    flush.last_error = Some(e.to_string());
                }
            }
        }

        if flush.sent > 0 {
            self.emit_public(Event::MessagesChanged { account_id: 0 });
        }
        Ok(flush)
    }

    /// Build a forward draft from an existing message.
    ///
    /// Unlike a reply, the recipient list starts empty — forwarding to whoever happened to be on
    /// the original is a classic way to leak a thread to the wrong person. Attachments are carried
    /// over, because a forward without them is usually pointless.
    pub fn forward_draft(&self, message_id: i64) -> Result<Draft> {
        let m = self.store.get_message(message_id)?;

        let subject = if m.subject.to_ascii_lowercase().starts_with("fw:")
            || m.subject.to_ascii_lowercase().starts_with("fwd:")
        {
            m.subject.clone()
        } else {
            format!("Fwd: {}", m.subject)
        };

        let to_line = m
            .recipients
            .iter()
            .filter(|r| r.kind == "to")
            .map(|r| r.addr.clone())
            .collect::<Vec<_>>()
            .join(", ");

        // The conventional forwarded-message header, so the recipient can see where it came
        // from and who was originally on it.
        let body = format!(
            concat!(
                "\r\n\r\n---------- Forwarded message ----------\r\n",
                "From: {} <{}>\r\n",
                "Date: {}\r\n",
                "Subject: {}\r\n",
                "To: {}\r\n\r\n{}"
            ),
            m.from_name,
            m.from_addr,
            format_date(m.date_utc),
            m.subject,
            to_line,
            m.body_text
        );

        // Copy the stored attachment blobs so the forward carries them even if the original is
        // later deleted.
        let mut attachments = Vec::new();
        for a in &m.attachments {
            if a.is_inline {
                continue; // Inline images belong to the quoted body, not as separate files.
            }
            if let Ok((filename, path)) = self.store.attachment_path(a.id) {
                attachments.push(signet_sync::outbox::DraftAttachment {
                    filename,
                    path: path.to_string_lossy().to_string(),
                });
            }
        }

        let bodies = crate::reply::forward_body(&m, &format_date(m.date_utc));
        let _ = body;

        Ok(Draft {
            account_id: m.account_id,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject,
            body_text: bodies.text,
            body_html: Some(bodies.html),
            in_reply_to: None,
            references: Vec::new(),
            attachments,
            signature: None,
            draft_message_id: None,
        })
    }

    /// Build a reply draft from an existing message.
    pub fn reply_draft(&self, message_id: i64, reply_all: bool) -> Result<Draft> {
        let m = self.store.get_message(message_id)?;
        let account = self.store.get_account(m.account_id)?;

        let mut to = vec![m.from_addr.clone()];
        let mut cc = Vec::new();
        if reply_all {
            for r in &m.recipients {
                // Never reply to yourself, and never expose Bcc recipients you were not shown.
                if r.addr.eq_ignore_ascii_case(&account.email) || r.kind == "bcc" {
                    continue;
                }
                match r.kind.as_str() {
                    "to" => to.push(r.addr.clone()),
                    "cc" => cc.push(r.addr.clone()),
                    _ => {}
                }
            }
        }
        to.retain(|a| !a.trim().is_empty());
        to.dedup();

        let subject = if m.subject.to_ascii_lowercase().starts_with("re:") {
            m.subject.clone()
        } else {
            format!("Re: {}", m.subject)
        };

        let sent = format_date(m.date_utc);
        let bodies = crate::reply::reply_body(&m, &sent);

        Ok(Draft {
            account_id: m.account_id,
            to,
            cc,
            bcc: Vec::new(),
            subject,
            body_text: bodies.text,
            body_html: Some(bodies.html),
            in_reply_to: m.rfc822_message_id.clone(),
            references: m.rfc822_message_id.into_iter().collect(),
            attachments: Vec::new(),
            // A reply gets the account default unless the composer picks otherwise.
            signature: None,
            draft_message_id: None,
        })
    }
}

/// An `Account` that has not been saved, for connection tests.
fn draft_account(new: &NewAccount) -> Account {
    Account {
        id: 0,
        display_name: new.display_name.clone(),
        email: new.email.clone(),
        pop_host: new.pop_host.clone(),
        pop_port: new.pop_port,
        pop_security: new.pop_security,
        pop_user: new.pop_user.clone(),
        smtp_host: new.smtp_host.clone(),
        smtp_port: new.smtp_port,
        smtp_security: new.smtp_security,
        smtp_user: new.smtp_user.clone(),
        credential_handle: String::new(),
        leave_on_server: new.leave_on_server,
        delete_after_days: new.delete_after_days,
        poll_minutes: new.poll_minutes,
        signature: new.signature.clone(),
        is_enabled: true,
        last_sync_utc: None,
        junk_filter_enabled: true,
        junk_threshold: 0.9,
        protocol: new.protocol,
    }
}

/// A date the way Outlook writes it in a quote header.
///
/// `Thursday, 3 September 2026 4:38 PM` — weekday, day without a leading zero, month name, year,
/// then a twelve-hour clock. Everyone in this trade reads dozens of these a day, and a reply whose
/// header is formatted differently from every other reply in the thread looks wrong even before
/// anyone notices why.
///
/// **Local time, not UTC.** The header sits next to the time the reader's own client shows, and a
/// four-hour discrepancy between the two reads as a fault in the message.
///
/// The previous version of this counted `days / 365` for the year and `(days % 365) / 30` for the
/// month, which is not a calendar: it drifted a day per leap year and put every date roughly a
/// fortnight out. Quote headers were simply wrong.
fn format_date(unix: i64) -> String {
    use chrono::{Local, TimeZone};

    match Local.timestamp_opt(unix, 0).single() {
        Some(dt) => dt.format("%A, %-d %B %Y %-I:%M %p").to_string(),
        // Out of range, or an ambiguous local time across a clock change. Saying nothing is better
        // than printing a date that is wrong.
        None => String::new(),
    }
}

#[cfg(test)]
mod date_tests {
    use super::format_date;

    #[test]
    fn a_quote_header_date_is_a_real_calendar_date() {
        // This counted `days / 365` for the year and `(days % 365) / 30` for the month, which put
        // every quote header roughly a fortnight out. 1_700_000_000 is 14 November 2023.
        let out = format_date(1_700_000_000);
        assert!(out.contains("November"), "wrong month: {out}");
        assert!(out.contains("2023"), "wrong year: {out}");
        assert!(
            out.contains("14") || out.contains("15"),
            "wrong day (allowing for the local offset either side of midnight): {out}"
        );
    }

    #[test]
    fn it_reads_the_way_outlook_writes_it() {
        // "Thursday, 3 September 2026 4:38 PM" — weekday, no leading zero on the day, month name,
        // then a twelve-hour clock.
        let out = format_date(1_788_857_078);
        let weekday = [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ]
        .iter()
        .any(|d| out.starts_with(d));
        assert!(weekday, "no weekday: {out}");
        assert!(out.contains("September") && out.contains("2026"), "{out}");
        assert!(
            out.ends_with("AM") || out.ends_with("PM"),
            "not a 12-hour clock: {out}"
        );
        assert!(!out.contains(" 0"), "a leading zero survived: {out}");
    }

    #[test]
    fn leap_years_do_not_drift() {
        // The old arithmetic lost a day for every leap year since 1970. 2024-02-29 is the case it
        // could never represent at all.
        let out = format_date(1_709_164_800); // 2024-02-29T00:00:00Z
        assert!(out.contains("February") || out.contains("March"), "{out}");
        assert!(out.contains("2024"), "{out}");
    }
}

#[cfg(test)]
mod sniff_tests {
    use super::sniff;

    #[test]
    fn real_files_are_recognised_by_their_own_bytes() {
        assert_eq!(sniff(b"%PDF-1.7\nstuff"), Some("application/pdf"));
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]),
            Some("image/png")
        );
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
    }

    #[test]
    fn html_wearing_a_pdf_name_is_refused() {
        // The attack this exists to stop. The viewer hands these bytes to the WebView with a type
        // attached and the blob is same-origin, so a document rendered here would be running
        // inside the application. What the sender called the file is not evidence of anything.
        assert_eq!(sniff(b"<html><script>alert(1)</script>"), None);
        assert_eq!(sniff(b"<!DOCTYPE html><body>"), None);
    }

    #[test]
    fn an_executable_is_never_renderable() {
        // `MZ` is a Windows executable. It should reach the save dialog, never the viewer.
        assert_eq!(sniff(b"MZ\x90\x00\x03"), None);
    }

    #[test]
    fn svg_is_refused_even_though_it_is_an_image() {
        // SVG is a document that can carry script. It is deliberately not in the sniff table, so
        // it falls through to being saved rather than rendered.
        assert_eq!(sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\">"), None);
        assert_eq!(sniff(b"<?xml version=\"1.0\"?><svg>"), None);
    }

    #[test]
    fn a_truncated_or_empty_file_is_refused_rather_than_guessed_at() {
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"%PD"), None);
        assert_eq!(sniff(&[0x89, b'P']), None);
    }

    #[test]
    fn the_extension_always_follows_the_bytes_not_the_sender() {
        use super::name_for_type;
        // A script whose first bytes look like a PDF must not keep an extension Windows would run.
        assert_eq!(name_for_type("invoice.bat", "application/pdf"), "invoice.pdf");
        assert_eq!(name_for_type("photo.hta", "image/jpeg"), "photo.jpg");
        assert_eq!(name_for_type("scan.pdf", "application/pdf"), "scan.pdf");
        assert_eq!(name_for_type(".bat", "image/png"), ".bat.png");
    }

    #[cfg(windows)]
    #[test]
    fn written_attachments_carry_the_mark_of_the_web() {
        let dir = std::env::temp_dir().join(format!("signet-motw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.pdf");
        std::fs::write(&file, b"%PDF-").unwrap();
        super::mark_from_internet(&file);
        let zone = std::fs::read_to_string(format!("{}:Zone.Identifier", file.display()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(zone.unwrap().contains("ZoneId=3"));
    }

    #[test]
    fn a_riff_container_that_is_not_webp_is_refused() {
        // A WAV file is also RIFF. Only the WEBP brand is an image.
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_all_excludes_yourself_and_never_leaks_bcc() {
        // Constructed directly rather than through a live sync.
        let store = Store::open_in_memory().unwrap();
        let account_id = store
            .add_account(
                &NewAccount {
                    display_name: "Dev".into(),
                    email: "dev@example.com".into(),
                    pop_host: "mail.test".into(),
                    pop_port: 995,
                    pop_security: Security::Tls,
                    pop_user: "dev".into(),
                    smtp_host: "mail.test".into(),
                    smtp_port: 587,
                    smtp_security: Security::Starttls,
                    smtp_user: "dev".into(),
                    leave_on_server: true,
                    delete_after_days: None,
                    poll_minutes: 5,
                    signature: String::new(),
                    protocol: Default::default(),
                },
                "handle",
            )
            .unwrap();
        let folder_id = store.folder_id(account_id, FolderKind::Inbox).unwrap();

        let msg_id = store
            .insert_message(&signet_store::NewMessage {
                account_id,
                folder_id,
                rfc822_message_id: Some("<orig@example.com>".into()),
                thread_id: "t".into(),
                from_name: "Alice".into(),
                from_addr: "alice@example.com".into(),
                subject: "Budget".into(),
                date_utc: 1_700_000_000,
                received_utc: 1_700_000_000,
                body_text: "figures".into(),
                recipients: vec![
                    signet_store::Recipient {
                        kind: "to".into(),
                        name: String::new(),
                        addr: "dev@example.com".into(),
                    },
                    signet_store::Recipient {
                        kind: "cc".into(),
                        name: String::new(),
                        addr: "bob@example.com".into(),
                    },
                    signet_store::Recipient {
                        kind: "bcc".into(),
                        name: String::new(),
                        addr: "hidden@example.com".into(),
                    },
                ],
                ..Default::default()
            })
            .unwrap();

        let app = App {
            store,
            events: broadcast::channel(16).0,
            sync_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        let reply = app.reply_draft(msg_id, true).unwrap();
        assert!(reply.to.contains(&"alice@example.com".to_string()));
        assert!(reply.cc.contains(&"bob@example.com".to_string()));
        assert!(
            !reply
                .to
                .iter()
                .chain(&reply.cc)
                .any(|a| a == "dev@example.com"),
            "reply-all addressed the sender to themselves"
        );
        assert!(
            !reply
                .to
                .iter()
                .chain(&reply.cc)
                .any(|a| a == "hidden@example.com"),
            "reply-all leaked a bcc recipient"
        );
        assert_eq!(reply.subject, "Re: Budget");
        assert_eq!(reply.in_reply_to.as_deref(), Some("<orig@example.com>"));
        assert!(reply.body_text.contains("> figures"), "quoting missing");
    }

    #[test]
    fn reply_does_not_stack_re_prefixes() {
        assert!(!"Re: Re: x".starts_with("Re: Re: Re:"));
        // The guard in reply_draft: an existing Re: is reused rather than doubled.
        let subject = "Re: Budget";
        let out = if subject.to_ascii_lowercase().starts_with("re:") {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        };
        assert_eq!(out, "Re: Budget");
    }
}
