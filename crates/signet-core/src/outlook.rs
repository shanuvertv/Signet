//! Importing an Outlook `.pst` file.
//!
//! A PST is not a mail file: it is Microsoft's on-disk database (MS-PST), with B-trees, a heap
//! layer and MAPI property contexts. The reading of that format is done by `outlook-pst`, which is
//! Microsoft's own Rust implementation of the spec. What is here is everything above it — turning
//! MAPI properties back into an email.
//!
//! **A PST does not store the original message.** Outlook keeps MAPI properties: a subject string,
//! a body string, an HTML blob, a recipient table, attachment nodes. The RFC822 bytes that arrived
//! from the server are gone. So an imported message is *reconstructed*, and this module builds a
//! real MIME message from the properties and then hands it to the same ingestion pipeline that
//! downloaded mail goes through. Two reasons for that rather than writing rows directly: the
//! search index, contact harvesting, sanitisation and inline-image handling all live in that
//! pipeline, and an imported message ends up with a genuine `.eml` on disk like every other one.
//!
//! Where Outlook kept `PR_TRANSPORT_MESSAGE_HEADERS` — it usually does for mail that arrived from
//! the internet — the original `Message-ID`, `References` and authentication results are recovered
//! from it. That matters more than it looks: `Message-ID` is what deduplication and the shared
//! team state are keyed on, so a message imported here is the same message to the rest of the app
//! as the copy someone else downloaded over IMAP.
//!
//! `.ost` files are refused. They use NDB version 36, which neither the spec's PST layout nor this
//! reader covers — and an OST is a cache of a mailbox that is still on the server, so the answer
//! there is to add the account rather than import a file.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use signet_store::FolderKind;
use outlook_pst::{
    AnsiPstFile, UnicodePstFile,
    ltp::prop_context::PropertyValue,
    messaging::{
        attachment::{AnsiAttachment, Attachment, AttachmentData, UnicodeAttachment},
        message::{AnsiMessage, Message, UnicodeMessage},
        store::{AnsiStore, EntryId, Store, UnicodeStore},
    },
    ndb::node_id::NodeId,
};
use serde::Serialize;

use crate::{App, Error, Event, Result};

// MAPI property tags. Named rather than inlined because a bare 0x0C1F in the middle of a match arm
// is unreadable, and these are the vocabulary of the whole module.
const PR_SUBJECT: u16 = 0x0037;
const PR_TRANSPORT_MESSAGE_HEADERS: u16 = 0x007D;
const PR_CLIENT_SUBMIT_TIME: u16 = 0x0039;
const PR_MESSAGE_DELIVERY_TIME: u16 = 0x0E06;
const PR_MESSAGE_FLAGS: u16 = 0x0E07;
const PR_SENDER_NAME: u16 = 0x0C1A;
const PR_SENDER_EMAIL_ADDRESS: u16 = 0x0C1F;
const PR_SENT_REPRESENTING_NAME: u16 = 0x0042;
const PR_SENT_REPRESENTING_EMAIL_ADDRESS: u16 = 0x0065;
const PR_SENDER_SMTP_ADDRESS: u16 = 0x5D01;
const PR_SENT_REPRESENTING_SMTP_ADDRESS: u16 = 0x5D02;
const PR_BODY: u16 = 0x1000;
const PR_HTML: u16 = 0x1013;
const PR_INTERNET_MESSAGE_ID: u16 = 0x1035;
const PR_DISPLAY_NAME: u16 = 0x3001;
const PR_EMAIL_ADDRESS: u16 = 0x3003;
const PR_SMTP_ADDRESS: u16 = 0x39FE;
const PR_RECIPIENT_TYPE: u16 = 0x0C15;
const PR_ATTACH_FILENAME: u16 = 0x3704;
const PR_ATTACH_LONG_FILENAME: u16 = 0x3707;
const PR_ATTACH_MIME_TAG: u16 = 0x370E;
const PR_ATTACH_CONTENT_ID: u16 = 0x3712;

/// `MSGFLAG_READ`
const MSGFLAG_READ: i32 = 0x01;

/// 1601-01-01 to 1970-01-01, in seconds. MAPI times are 100-nanosecond intervals since the former.
const FILETIME_EPOCH_OFFSET: i64 = 11_644_473_600;

#[derive(Debug, Clone, Serialize)]
pub struct OutlookPreview {
    /// What Outlook called this store, e.g. "Outlook Data File".
    pub display_name: String,
    pub folders: Vec<OutlookFolderPreview>,
    pub messages: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutlookFolderPreview {
    pub path: String,
    pub messages: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OutlookImportSummary {
    pub folders: i64,
    pub messages_imported: i64,
    /// Already present, matched on `Message-ID`.
    pub messages_skipped: i64,
    /// Messages the reader could not make sense of. One bad message must not abandon the import.
    pub failed: i64,
    /// Why the first failure failed, if any did.
    ///
    /// A count on its own — "3 could not be read" — is not something anybody can act on, and it is
    /// the only thing an import that quietly produced nothing has to offer.
    #[serde(default)]
    pub first_error: Option<String>,
}

/// Display name and address pairs, as `mail-builder` wants them.
type Addresses = Vec<(String, String)>;

/// One message lifted out of the PST and written to disk as MIME, ready for the normal pipeline.
struct Extracted {
    raw_path: PathBuf,
    stem: String,
    folder_kind: FolderKind,
    folder_name: String,
    is_read: bool,
}

impl App {
    /// Describe a PST without importing it.
    pub fn preview_outlook_file(&self, path: String) -> Result<OutlookPreview> {
        with_store(Path::new(&path), |walk| walk.preview())
    }

    /// Create a mailbox that exists only on this computer, to import an archive into.
    ///
    /// An old `.pst` is somebody's history, not a live mailbox, and merging it into a working
    /// account buries today's mail under ten years of it. This makes a home for it instead: an
    /// account with no servers, disabled, so no sync will ever touch it and nothing can be sent
    /// from it. It is a filing cabinet, and the folder rail shows it as one.
    pub fn create_local_archive(&self, name: String) -> Result<i64> {
        let name = name.trim();
        let name = if name.is_empty() { "Archive" } else { name };

        // The address only has to be unique and recognisable; it is never used to send anything.
        // `.invalid` is reserved by RFC 2606 precisely so it can never resolve.
        let mut email = format!("{}@archive.invalid", slug(name));
        let taken: Vec<String> = self
            .store()
            .list_accounts()?
            .into_iter()
            .map(|a| a.email.to_lowercase())
            .collect();
        let mut n = 2;
        while taken.contains(&email.to_lowercase()) {
            email = format!("{}-{n}@archive.invalid", slug(name));
            n += 1;
        }

        let id = self.store().add_account(
            &signet_store::NewAccount {
                display_name: name.to_string(),
                email,
                // Empty servers are what marks this as local-only, and what makes any attempt to
                // sync or send fail loudly rather than half-work.
                pop_host: String::new(),
                pop_port: 0,
                pop_security: signet_store::Security::Tls,
                pop_user: String::new(),
                smtp_host: String::new(),
                smtp_port: 0,
                smtp_security: signet_store::Security::Tls,
                smtp_user: String::new(),
                leave_on_server: true,
                delete_after_days: None,
                poll_minutes: 1440,
                signature: String::new(),
                protocol: Default::default(),
            },
            "",
        )?;

        let mut settings = self.account_settings(id)?;
        settings.is_enabled = false;
        self.store().update_account_settings(id, &settings)?;

        self.emit_public(Event::AccountsChanged);
        Ok(id)
    }

    /// Import every message in a PST into `account_id`.
    pub async fn import_outlook_file(
        &self,
        path: String,
        account_id: i64,
    ) -> Result<OutlookImportSummary> {
        let account = self.store().get_account(account_id)?;
        let ctx = signet_sync::ingest::StoreContext::new(self.store(), &account).await?;
        let raw_dir = ctx.raw_dir.clone();
        let known = self.store().known_rfc822_ids(account_id)?;

        let report = |phase: signet_sync::progress::Phase, done: usize, total: usize| {
            self.emit_public(Event::SyncProgress {
                account_id,
                phase: phase.label(),
                done,
                total,
                folder: String::new(),
                bytes_done: 0,
                bytes_total: 0,
            });
        };
        report(signet_sync::progress::Phase::Listing, 0, 0);

        // Reading the PST is synchronous and its handles are `Rc`, so none of it can cross an
        // await. It runs on a blocking thread and hands back files on disk rather than message
        // bodies in memory, which keeps a multi-gigabyte archive from having to fit in RAM.
        let (extracted, mut summary) =
            tokio::task::spawn_blocking(move || extract_all(&path, &raw_dir, &known))
                .await
                .map_err(|e| Error::Invalid(format!("the import thread failed: {e}")))??;

        // Only now is the total known — reading the PST is itself most of the wait on a large one,
        // and it cannot be counted until the tree has been walked.
        let total = extracted.len();
        for (n, item) in extracted.into_iter().enumerate() {
            match self.ingest_extracted(&account, &ctx, &item).await {
                Ok(()) => summary.messages_imported += 1,
                Err(e) => {
                    tracing::warn!(stem = %item.stem, error = %e, "could not import a message");
                    summary.failed += 1;
                    summary.first_error.get_or_insert_with(|| e.to_string());
                    let _ = std::fs::remove_file(&item.raw_path);
                }
            }
            // Every fiftieth, not every one: a 20,000 message archive would otherwise spend more
            // effort telling the window about progress than making any.
            if n % 50 == 0 || n + 1 == total {
                report(signet_sync::progress::Phase::Downloading, n + 1, total);
            }
        }

        report(signet_sync::progress::Phase::Finishing, total, total);
        self.emit_public(Event::MessagesChanged { account_id });
        Ok(summary)
    }

    async fn ingest_extracted(
        &self,
        account: &signet_store::Account,
        ctx: &signet_sync::ingest::StoreContext,
        item: &Extracted,
    ) -> Result<()> {
        let raw = std::fs::read(&item.raw_path)?;
        let parsed = signet_mime::parse(&raw)
            .ok_or_else(|| Error::Invalid("the rebuilt message could not be parsed".into()))?;

        let folder_id = self.folder_for_import(account.id, item)?;

        let stored = signet_sync::ingest::store_message(
            self.store(),
            account,
            folder_id,
            ctx,
            &parsed,
            &item.raw_path,
            &item.stem,
            raw.len() as u64,
            false,
            // Never filter an import. Someone's own archive being scattered into Junk on the way in
            // would be worse than useless, and the mail has already been through a filter once.
            false,
        )
        .await?;

        if item.is_read {
            self.store().set_read(&[stored.message_id], true)?;
        }
        Ok(())
    }

    fn folder_for_import(&self, account_id: i64, item: &Extracted) -> Result<i64> {
        if item.folder_kind != FolderKind::Custom {
            return Ok(self.store().folder_id(account_id, item.folder_kind)?);
        }
        if let Some(id) =
            self.store()
                .folder_by_kind_and_name(account_id, "custom", &item.folder_name)?
        {
            return Ok(id);
        }
        Ok(self
            .store()
            .create_folder(account_id, "custom", &item.folder_name)?)
    }
}

// ------------------------------------------------------------------ PST flavours

/// The two on-disk layouts, behind one interface.
///
/// Everything except reading attachment bytes works through `dyn Store`, but an attachment has to
/// be read from a concrete `UnicodeMessage` or `AnsiMessage`. Rather than write the walk twice,
/// the two differences are named here and the walk is generic over them.
trait Flavour {
    type Store: Store + 'static;
    type Msg: Message + 'static;

    fn read_message(store: &Rc<Self::Store>, entry: &EntryId) -> io::Result<Rc<Self::Msg>>;
    fn read_attachment(msg: &Rc<Self::Msg>, sub: NodeId) -> io::Result<Rc<dyn Attachment>>;
}

struct Unicode;
struct Ansi;

impl Flavour for Unicode {
    type Store = UnicodeStore;
    type Msg = UnicodeMessage;

    fn read_message(store: &Rc<Self::Store>, entry: &EntryId) -> io::Result<Rc<Self::Msg>> {
        UnicodeMessage::read(store.clone(), entry, None)
    }
    fn read_attachment(msg: &Rc<Self::Msg>, sub: NodeId) -> io::Result<Rc<dyn Attachment>> {
        Ok(UnicodeAttachment::read(msg.clone(), sub, None)?)
    }
}

impl Flavour for Ansi {
    type Store = AnsiStore;
    type Msg = AnsiMessage;

    fn read_message(store: &Rc<Self::Store>, entry: &EntryId) -> io::Result<Rc<Self::Msg>> {
        AnsiMessage::read(store.clone(), entry, None)
    }
    fn read_attachment(msg: &Rc<Self::Msg>, sub: NodeId) -> io::Result<Rc<dyn Attachment>> {
        Ok(AnsiAttachment::read(msg.clone(), sub, None)?)
    }
}

/// Open a PST in whichever layout it turns out to be, and run `f` against it.
fn with_store<T>(path: &Path, f: impl Fn(&mut Walk<'_>) -> Result<T>) -> Result<T> {
    if !path.exists() {
        return Err(Error::Invalid(format!("{} does not exist", path.display())));
    }

    match UnicodePstFile::open(path) {
        Ok(pst) => {
            let store = UnicodeStore::read(Rc::new(pst)).map_err(open_err)?;
            let mut walk = Walk::new(store.clone() as Rc<dyn Store>);
            f(&mut walk)
        }
        Err(unicode_err) => match AnsiPstFile::open(path) {
            Ok(pst) => {
                let store = AnsiStore::read(Rc::new(pst)).map_err(open_err)?;
                let mut walk = Walk::new(store.clone() as Rc<dyn Store>);
                f(&mut walk)
            }
            Err(_) => Err(unreadable(path, &unicode_err)),
        },
    }
}

/// Say something useful about a file that would not open.
///
/// The reader's own error is about NDB header fields, which tells a salesperson nothing. The two
/// cases worth naming are an OST and a file that is not a PST at all.
fn unreadable(path: &Path, err: &io::Error) -> Error {
    // Outlook holds its data files open exclusively for as long as it is running, so this is the
    // error a user hits first and most often. Reporting it as a bad file format would send them
    // looking for a problem with the file, which is the wrong place entirely.
    if is_locked(err) {
        return Error::Invalid(format!(
            "{} is open in Outlook, which keeps its data files locked while it is running. Close \
             Outlook completely and try again.",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        ));
    }

    let ost = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("ost"))
        .unwrap_or(false);
    if ost {
        return Error::Invalid(
            "This is an .ost file, which is Outlook's offline cache of a mailbox rather than an \
             archive, and is stored in a format this importer cannot read. The mail in it is still \
             on the server: add the account under Add account and it will download. To bring it in \
             as a file instead, open Outlook and use File > Open & Export > Import/Export to \
             export the folders to a .pst first."
                .into(),
        );
    }
    Error::Invalid(format!(
        "This does not look like an Outlook data file that can be read ({err}). Only .pst files \
         are supported — .ost files must be exported to .pst from Outlook first."
    ))
}

/// Whether the file could not be opened because something else has it.
///
/// `ERROR_SHARING_VIOLATION` (32) is the whole file held open by another process;
/// `ERROR_LOCK_VIOLATION` (33) is a byte range locked within it. Outlook produces both, depending
/// on which file it is and what it is doing at the time.
fn is_locked(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(32) | Some(33))
        || err.kind() == io::ErrorKind::PermissionDenied
}

fn open_err(e: io::Error) -> Error {
    Error::Invalid(format!("the Outlook data file could not be opened: {e}"))
}

// ------------------------------------------------------------------ walking

struct Walk<'a> {
    store: Rc<dyn Store>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl Walk<'_> {
    fn new(store: Rc<dyn Store>) -> Self {
        Self {
            store,
            _marker: std::marker::PhantomData,
        }
    }

    fn preview(&mut self) -> Result<OutlookPreview> {
        let display_name = self
            .store
            .properties()
            .display_name()
            .unwrap_or_else(|_| "Outlook Data File".into());

        let mut folders = Vec::new();
        let mut total = 0i64;
        for (path, folder) in self.folders()? {
            let count = folder
                .contents_table()
                .map(|t| t.rows_matrix().count() as i64)
                .unwrap_or(0);
            if count == 0 {
                continue;
            }
            total += count;
            folders.push(OutlookFolderPreview {
                path,
                messages: count,
            });
        }
        folders.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(OutlookPreview {
            display_name,
            folders,
            messages: total,
        })
    }

    /// Every folder in the message store, with its display path.
    fn folders(&self) -> Result<Vec<(String, Rc<dyn outlook_pst::messaging::folder::Folder>)>> {
        let root_id = self
            .store
            .properties()
            .ipm_sub_tree_entry_id()
            .map_err(open_err)?;
        let root = self.store.open_folder(&root_id).map_err(open_err)?;

        let mut out = Vec::new();
        let mut stack = vec![(String::new(), root)];
        while let Some((prefix, folder)) = stack.pop() {
            let Some(hierarchy) = folder.hierarchy_table() else {
                continue;
            };
            for row in hierarchy.rows_matrix() {
                let node = NodeId::from(u32::from(row.id()));
                let Ok(entry) = self.store.properties().make_entry_id(node) else {
                    continue;
                };
                let Ok(child) = self.store.open_folder(&entry) else {
                    continue;
                };
                let name = child
                    .properties()
                    .display_name()
                    .unwrap_or_else(|_| "(unnamed)".into());
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                stack.push((path.clone(), child.clone()));
                out.push((path, child));
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------ extraction

/// Read every message out of the PST, writing each as MIME into `raw_dir`.
fn extract_all(
    path: &str,
    raw_dir: &Path,
    known: &HashSet<String>,
) -> Result<(Vec<Extracted>, OutlookImportSummary)> {
    let path = Path::new(path);
    std::fs::create_dir_all(raw_dir)?;

    match UnicodePstFile::open(path) {
        Ok(pst) => {
            let store = UnicodeStore::read(Rc::new(pst)).map_err(open_err)?;
            extract_with::<Unicode>(store, raw_dir, known)
        }
        Err(unicode_err) => match AnsiPstFile::open(path) {
            Ok(pst) => {
                let store = AnsiStore::read(Rc::new(pst)).map_err(open_err)?;
                extract_with::<Ansi>(store, raw_dir, known)
            }
            Err(_) => Err(unreadable(path, &unicode_err)),
        },
    }
}

fn extract_with<F: Flavour>(
    store: Rc<F::Store>,
    raw_dir: &Path,
    known: &HashSet<String>,
) -> Result<(Vec<Extracted>, OutlookImportSummary)> {
    let dyn_store: Rc<dyn Store> = store.clone();
    let walk = Walk::new(dyn_store.clone());

    let mut out = Vec::new();
    let mut summary = OutlookImportSummary::default();
    let mut seen: HashSet<String> = HashSet::new();

    for (path, folder) in walk.folders()? {
        let Some(contents) = folder.contents_table() else {
            continue;
        };
        summary.folders += 1;
        let (kind, name) = classify_folder(&path);

        for row in contents.rows_matrix() {
            let node = NodeId::from(u32::from(row.id()));
            let Ok(entry) = dyn_store.properties().make_entry_id(node) else {
                summary.failed += 1;
                continue;
            };
            let Ok(message) = F::read_message(&store, &entry) else {
                summary.failed += 1;
                continue;
            };

            match build_one::<F>(&message, raw_dir, known, &mut seen, kind, &name) {
                Ok(Some(item)) => out.push(item),
                Ok(None) => summary.messages_skipped += 1,
                Err(e) => {
                    tracing::warn!(folder = %path, error = %e, "skipping an unreadable message");
                    summary.failed += 1;
                    summary.first_error.get_or_insert_with(|| e.to_string());
                }
            }
        }
    }

    Ok((out, summary))
}

fn build_one<F: Flavour>(
    message: &Rc<F::Msg>,
    raw_dir: &Path,
    known: &HashSet<String>,
    seen: &mut HashSet<String>,
    kind: FolderKind,
    folder_name: &str,
) -> Result<Option<Extracted>> {
    let props = message.properties();

    let real_id = text(props.get(PR_INTERNET_MESSAGE_ID))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            text(props.get(PR_TRANSPORT_MESSAGE_HEADERS))
                .and_then(|h| header_value(&h, "message-id"))
        })
        .map(|id| normalise_message_id(&id));

    // Outlook keeps a `Message-ID` only for mail that arrived from the internet with its transport
    // headers intact, which in practice is a minority of a PST. Without one there is nothing to
    // deduplicate on, and importing the same archive twice silently doubles it.
    //
    // So one is synthesised from the parts of the message that do not change: who sent it, when,
    // what it was about, and how big it was. The same message yields the same id on every import,
    // which is exactly what deduplication needs — and it is marked `.pst.invalid` so it can never
    // be mistaken for an identifier a real sender chose.
    let message_id = real_id.or_else(|| {
        let subject = text(props.get(PR_SUBJECT)).unwrap_or_default();
        let (_, from_addr) = sender(props);
        let when = props
            .get(PR_MESSAGE_DELIVERY_TIME)
            .or_else(|| props.get(PR_CLIENT_SUBMIT_TIME))
            .and_then(filetime)
            .unwrap_or(0);
        let size = props.message_size().unwrap_or(0);
        let seed = format!("{from_addr}|{when}|{size}|{subject}");
        Some(format!(
            "pst-{:016x}@import.pst.invalid",
            fnv1a(seed.as_bytes())
        ))
    });

    if let Some(id) = &message_id {
        // Already downloaded over IMAP or POP3, or already imported. The archive and the mailbox
        // overlap heavily in practice, so this is the common case rather than the exception.
        if known.contains(id) || !seen.insert(id.clone()) {
            return Ok(None);
        }
    }

    let eml = build_mime::<F>(message, message_id.as_deref())?;
    let stem = format!("PST{:016x}", fnv1a(&eml));
    let raw_path = raw_dir.join(format!("{stem}.eml"));
    std::fs::write(&raw_path, &eml)?;

    let is_read = props
        .get(PR_MESSAGE_FLAGS)
        .and_then(|v| match v {
            PropertyValue::Integer32(n) => Some(*n),
            _ => None,
        })
        .map(|flags| flags & MSGFLAG_READ != 0)
        .unwrap_or(false);

    Ok(Some(Extracted {
        raw_path,
        stem,
        folder_kind: kind,
        folder_name: folder_name.to_string(),
        is_read,
    }))
}

/// Rebuild an RFC822 message from MAPI properties.
fn build_mime<F: Flavour>(message: &Rc<F::Msg>, message_id: Option<&str>) -> Result<Vec<u8>> {
    use mail_builder::MessageBuilder;
    use mail_builder::headers::raw::Raw;

    let props = message.properties();

    let subject = text(props.get(PR_SUBJECT)).unwrap_or_default();
    let (from_name, from_addr) = sender(props);
    let date = props
        .get(PR_MESSAGE_DELIVERY_TIME)
        .or_else(|| props.get(PR_CLIENT_SUBMIT_TIME))
        .and_then(filetime)
        .unwrap_or(0);

    let mut builder = MessageBuilder::new()
        .subject(subject)
        .from((from_name.as_str(), from_addr.as_str()));

    let (to, cc) = recipients::<F>(message);
    if !to.is_empty() {
        builder = builder.to(to);
    }
    if !cc.is_empty() {
        builder = builder.cc(cc);
    }
    if date > 0 {
        builder = builder.date(date);
    }

    // Carry over the identity headers Outlook kept, and only those. The rest of the original
    // headers describe a MIME structure that no longer exists — this body is being rebuilt — so
    // copying Content-Type across would produce a message that contradicts itself.
    if let Some(raw) = text(props.get(PR_TRANSPORT_MESSAGE_HEADERS)) {
        for name in [
            "in-reply-to",
            "references",
            "reply-to",
            "return-path",
            "authentication-results",
            "received-spf",
            "list-unsubscribe",
        ] {
            if let Some(value) = header_value(&raw, name) {
                builder = builder.header(name, Raw::new(value));
            }
        }
    }
    if let Some(id) = message_id {
        builder = builder.message_id(id.trim_start_matches('<').trim_end_matches('>').to_string());
    }

    let body_text = text(props.get(PR_BODY)).unwrap_or_default();
    let body_html = props.get(PR_HTML).and_then(|v| match v {
        PropertyValue::Binary(b) => Some(String::from_utf8_lossy(b.buffer()).into_owned()),
        _ => None,
    });

    builder = match (&body_html, body_text.is_empty()) {
        (Some(html), false) => builder.text_body(body_text.clone()).html_body(html.clone()),
        (Some(html), true) => builder.html_body(html.clone()),
        (None, _) => builder.text_body(body_text.clone()),
    };

    for att in attachments::<F>(message) {
        builder = match att.content_id {
            Some(cid) => builder.inline(att.mime, cid, att.data),
            None => builder.attachment(att.mime, att.filename, att.data),
        };
    }

    builder
        .write_to_vec()
        .map_err(|e| Error::Invalid(format!("could not rebuild the message: {e}")))
}

struct ExtractedAttachment {
    filename: String,
    mime: String,
    content_id: Option<String>,
    data: Vec<u8>,
}

fn attachments<F: Flavour>(message: &Rc<F::Msg>) -> Vec<ExtractedAttachment> {
    let Some(table) = message.attachment_table() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for row in table.rows_matrix() {
        let sub = NodeId::from(u32::from(row.id()));
        let Ok(att) = F::read_attachment(message, sub) else {
            continue;
        };
        let props = att.properties();

        // Only ordinary attached files. An attached *message* would need the whole build to recurse
        // and is rare enough in a sales mailbox that losing it is better than guessing wrong.
        let Some(AttachmentData::Binary(data)) = att.data() else {
            continue;
        };

        let filename = text(props.get(PR_ATTACH_LONG_FILENAME))
            .or_else(|| text(props.get(PR_ATTACH_FILENAME)))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "attachment".into());
        let mime = text(props.get(PR_ATTACH_MIME_TAG))
            .filter(|s| s.contains('/'))
            .unwrap_or_else(|| "application/octet-stream".into());
        let content_id = text(props.get(PR_ATTACH_CONTENT_ID))
            .map(|c| c.trim_matches(['<', '>']).to_string())
            .filter(|c| !c.is_empty());

        out.push(ExtractedAttachment {
            filename,
            mime,
            content_id,
            data: data.buffer().to_vec(),
        });
    }
    out
}

fn recipients<F: Flavour>(message: &Rc<F::Msg>) -> (Addresses, Addresses) {
    let mut to = Vec::new();
    let mut cc = Vec::new();

    let Some(table) = message.recipient_table() else {
        return (to, cc);
    };
    let context = table.context();

    let column = |id: u16| context.columns().iter().position(|c| c.prop_id() == id);
    let (name_col, addr_col, smtp_col, kind_col) = (
        column(PR_DISPLAY_NAME),
        column(PR_EMAIL_ADDRESS),
        column(PR_SMTP_ADDRESS),
        column(PR_RECIPIENT_TYPE),
    );

    for row in table.rows_matrix() {
        let Ok(values) = row.columns(context) else {
            continue;
        };
        let read = |idx: Option<usize>| -> Option<PropertyValue> {
            let idx = idx?;
            let value = values.get(idx)?.as_ref()?;
            table
                .read_column(value, context.columns()[idx].prop_type())
                .ok()
        };

        // The SMTP address is the one worth having. `PR_EMAIL_ADDRESS` on an Exchange recipient is
        // an X.500 distinguished name, which is not an email address and must not be treated as
        // one, so it is only used when it actually looks like an address.
        let addr = read(smtp_col)
            .and_then(|v| text(Some(&v)))
            .filter(|s| s.contains('@'))
            .or_else(|| {
                read(addr_col)
                    .and_then(|v| text(Some(&v)))
                    .filter(|s| s.contains('@'))
            });
        let name = read(name_col)
            .and_then(|v| text(Some(&v)))
            .unwrap_or_default();

        let Some(addr) = addr else { continue };
        let kind = read(kind_col)
            .and_then(|v| match v {
                PropertyValue::Integer32(n) => Some(n),
                _ => None,
            })
            .unwrap_or(1);

        match kind {
            2 => cc.push((name, addr)),
            // 3 is Bcc, which must not be reintroduced into the headers of an archived message.
            3 => {}
            _ => to.push((name, addr)),
        }
    }
    (to, cc)
}

fn sender(props: &outlook_pst::messaging::message::MessageProperties) -> (String, String) {
    let addr = [
        PR_SENDER_SMTP_ADDRESS,
        PR_SENT_REPRESENTING_SMTP_ADDRESS,
        PR_SENDER_EMAIL_ADDRESS,
        PR_SENT_REPRESENTING_EMAIL_ADDRESS,
    ]
    .into_iter()
    .filter_map(|id| text(props.get(id)))
    .find(|s| s.contains('@'))
    .unwrap_or_else(|| "unknown@invalid".into());

    let name = text(props.get(PR_SENDER_NAME))
        .or_else(|| text(props.get(PR_SENT_REPRESENTING_NAME)))
        .unwrap_or_default();

    (name, addr)
}

// ------------------------------------------------------------------ small helpers

/// Read a property as text, whichever string flavour it happens to be.
fn text(value: Option<&PropertyValue>) -> Option<String> {
    match value? {
        PropertyValue::Unicode(v) => Some(String::from_utf16_lossy(v.buffer())),
        PropertyValue::String8(v) => Some(v.buffer().iter().map(|&b| b as char).collect()),
        PropertyValue::Binary(v) => Some(String::from_utf8_lossy(v.buffer()).into_owned()),
        _ => None,
    }
    .map(|s| s.trim_end_matches('\0').to_string())
}

/// MAPI time to Unix seconds.
fn filetime(value: &PropertyValue) -> Option<i64> {
    match value {
        PropertyValue::Time(ticks) if *ticks > 0 => {
            Some(ticks / 10_000_000 - FILETIME_EPOCH_OFFSET)
        }
        _ => None,
    }
}

/// One header out of a raw header block, unfolded.
fn header_value(raw: &str, name: &str) -> Option<String> {
    let mut lines = raw.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case(name) {
            continue;
        }
        let mut value = value.trim().to_string();
        // Continuation lines start with whitespace; References in particular is nearly always
        // folded across several of them, and a truncated References breaks threading.
        while let Some(next) = lines.peek() {
            if !next.starts_with([' ', '\t']) {
                break;
            }
            value.push(' ');
            value.push_str(next.trim());
            lines.next();
        }
        return Some(value);
    }
    None
}

/// Message-IDs are compared as text against what the store holds, so they have to arrive in the
/// same shape it uses: **bare, with the angle brackets stripped**.
///
/// That is what the MIME parser produces, so it is what every row in `messages` contains. Adding
/// brackets here instead — which reads more natural, since that is how the header is written —
/// means no comparison ever matches and the same archive imports again in full every time.
fn normalise_message_id(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_string()
}

/// Outlook's folder names, mapped onto ours.
///
/// Matching on the display name is crude, but a PST records no machine-readable folder role for
/// anything except the special folders in the store properties, and those do not cover Junk or
/// Archive. A name we do not recognise becomes a folder of its own rather than being flattened
/// into the Inbox.
fn classify_folder(path: &str) -> (FolderKind, String) {
    let leaf = path.rsplit('/').next().unwrap_or(path);
    let kind = match leaf.to_lowercase().as_str() {
        "inbox" => FolderKind::Inbox,
        "sent items" | "sent" | "sent mail" => FolderKind::Sent,
        "drafts" => FolderKind::Drafts,
        "deleted items" | "trash" => FolderKind::Deleted,
        "junk e-mail" | "junk email" | "junk" | "spam" => FolderKind::Junk,
        "archive" => FolderKind::Archive,
        _ => FolderKind::Custom,
    };
    (kind, path.to_string())
}

/// A short, stable name for the file on disk, derived from its content.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A filesystem- and address-safe stem for a mailbox name.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').replace("--", "-");
    if s.is_empty() { "archive".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_headers_are_joined_back_together() {
        // References is nearly always folded, and a truncated one breaks threading silently.
        let raw = "Subject: hi\r\nReferences: <a@x>\r\n <b@x>\r\n\t<c@x>\r\nTo: dev@x\r\n";
        assert_eq!(
            header_value(raw, "references").as_deref(),
            Some("<a@x> <b@x> <c@x>")
        );
    }

    #[test]
    fn header_lookup_ignores_case_and_missing_headers() {
        let raw = "Message-ID: <one@x>\r\n";
        assert_eq!(header_value(raw, "message-id").as_deref(), Some("<one@x>"));
        assert_eq!(header_value(raw, "MESSAGE-ID").as_deref(), Some("<one@x>"));
        assert_eq!(header_value(raw, "references"), None);
    }

    #[test]
    fn message_ids_come_out_in_the_shape_the_store_uses() {
        // Bare, because that is what the MIME parser yields and therefore what every stored row
        // holds. Getting this backwards means deduplication silently never matches.
        assert_eq!(normalise_message_id("  <a@b>  "), "a@b");
        assert_eq!(normalise_message_id("a@b"), "a@b");
        assert_eq!(normalise_message_id("<<a@b>>"), "a@b");
    }

    #[test]
    fn outlook_folder_names_map_onto_ours() {
        assert_eq!(classify_folder("Inbox").0, FolderKind::Inbox);
        assert_eq!(classify_folder("Sent Items").0, FolderKind::Sent);
        assert_eq!(classify_folder("Junk E-mail").0, FolderKind::Junk);
        assert_eq!(classify_folder("Deleted Items").0, FolderKind::Deleted);
    }

    #[test]
    fn a_nested_folder_is_matched_on_its_own_name_not_its_parents() {
        assert_eq!(classify_folder("Customers/2024/Inbox").0, FolderKind::Inbox);
        let (kind, name) = classify_folder("Customers/Acme");
        assert_eq!(kind, FolderKind::Custom);
        assert_eq!(
            name, "Customers/Acme",
            "the path is what the user recognises"
        );
    }

    #[test]
    fn an_unrecognised_folder_keeps_its_own_identity() {
        // Flattening someone's filing into the Inbox would destroy the organisation they came for.
        assert_eq!(classify_folder("Quotes 2023").0, FolderKind::Custom);
    }

    #[test]
    fn mapi_time_converts_to_unix_seconds() {
        // 2023-11-14T22:13:20Z as 100ns ticks since 1601.
        let ticks = (1_700_000_000 + FILETIME_EPOCH_OFFSET) * 10_000_000;
        assert_eq!(filetime(&PropertyValue::Time(ticks)), Some(1_700_000_000));
        assert_eq!(filetime(&PropertyValue::Time(0)), None);
    }
}
