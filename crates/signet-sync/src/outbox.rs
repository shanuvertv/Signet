//! The outbox: composing writes a queued row, a worker drains it.
//!
//! Every send goes through here rather than straight to SMTP. That is what gives offline compose,
//! retry with backoff, a visible "Sending…" state, and — because no server does it for us — a
//! reliable copy in the local Sent folder.

use signet_store::{FolderKind, NewAttachment, NewMessage, Recipient, Store};
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::mime::MimePart;

use crate::{Error, Result};

/// A file the user attached, still at the path they picked it from.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftAttachment {
    pub filename: String,
    pub path: String,
}

/// Attachments are capped so one oversized file cannot produce a message the server will refuse
/// after we have already spent minutes uploading it. 25 MB is the common limit.
pub const MAX_ATTACHMENT_TOTAL: u64 = 25 * 1024 * 1024;

/// Best-effort content type from the file extension.
///
/// Wrong only in ways that do not matter: receiving clients sniff anyway, and the fallback of
/// `application/octet-stream` is always safe.
pub fn guess_mime(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "dwg" => "image/vnd.dwg",
        "ies" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// A message the user has composed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Draft {
    pub account_id: i64,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    /// The rich version, when there is one.
    ///
    /// Almost all business mail is HTML, and a reply that flattens the original to text and
    /// prefixes every line with `>` destroys tables and breaks links apart. When this is set the
    /// message goes out as `multipart/alternative` with `body_text` as the fallback.
    #[serde(default)]
    pub body_html: Option<String>,
    /// Set when replying, so the thread survives in the recipient's client.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<DraftAttachment>,
    /// What to append as a signature.
    ///
    /// `None` means "use the account's default", which is what the CLI and older callers expect.
    /// `Some("")` means the composer already put a signature in the body and appending again
    /// would duplicate it — the case that matters once the user can pick one in the UI.
    #[serde(default)]
    pub signature: Option<String>,
    /// Set when the draft came from the Drafts folder, so sending replaces it rather than
    /// leaving a stale copy behind.
    #[serde(default)]
    pub draft_message_id: Option<i64>,
}

impl Draft {
    fn all_entries(&self) -> impl Iterator<Item = &String> {
        self.to.iter().chain(self.cc.iter()).chain(self.bcc.iter())
    }

    /// Every envelope recipient, including Bcc: bare addresses, each once.
    ///
    /// The SMTP server is given `a@x.org`, never `Name <a@x.org>`, which it rejects.
    pub fn envelope_recipients(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .all_entries()
            .filter_map(|e| parse_mailbox(e).map(|(_, addr)| addr))
            .collect();
        all.sort_by_key(|a| a.to_lowercase());
        all.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        all
    }

    /// Entries that are not an email address, exactly as the person typed them.
    pub fn invalid_recipients(&self) -> Vec<String> {
        self.all_entries()
            .map(|e| e.trim())
            .filter(|e| !e.is_empty() && parse_mailbox(e).is_none())
            .map(str::to_string)
            .collect()
    }
}

/// One recipient as people type or paste it: `a@x.org`, `<a@x.org>`, `Name <a@x.org>`,
/// `"Doe, John" <a@x.org>`. Returns the display name, if any, and the address; `None` when there
/// is no plausible address in it.
///
/// Passing the whole entry through as an address was why mail pasted from Outlook did not go: the
/// header came out as `<Name <a@x.org>>` and the server was asked to deliver to `Name <a@x.org>`.
pub fn parse_mailbox(entry: &str) -> Option<(Option<String>, String)> {
    let entry = entry.trim();
    let (name, addr) = match (entry.rfind('<'), entry.rfind('>')) {
        (Some(open), Some(close)) if open < close => {
            let name = entry[..open].trim().trim_matches('"').trim();
            let name = (!name.is_empty()).then(|| name.to_string());
            (name, entry[open + 1..close].trim())
        }
        _ => (None, entry),
    };
    is_plausible_address(addr).then(|| (name, addr.to_string()))
}

/// Shape only (something@domain.tld, no spaces or separators): enough to catch typos and pasting
/// accidents before the server does, without rejecting any address a real server would accept.
fn is_plausible_address(addr: &str) -> bool {
    let Some((local, domain)) = addr.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !addr
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"'))
}

/// A header's worth of recipients, each with its display name when it had one.
fn address_list(entries: &[String]) -> Address<'static> {
    Address::new_list(
        entries
            .iter()
            .filter_map(|e| parse_mailbox(e))
            .map(|(name, addr)| Address::new_address(name, addr))
            .collect(),
    )
}

/// A Message-ID on the sender's own domain.
///
/// The builder's default ends in the computer's name (`@DESKTOP-1234`), which is not a domain at
/// all and which spam filters count against a message.
fn message_id(from_addr: &str) -> String {
    use std::hash::{BuildHasher, RandomState};
    let domain = from_addr.rsplit_once('@').map_or("localhost", |(_, d)| d);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}.{:x}@{domain}", RandomState::new().hash_one(nanos))
}

/// Build the RFC822 bytes for a draft.
///
/// Bcc is deliberately **not** written as a header. It belongs to the SMTP envelope only;
/// emitting it here would disclose the blind recipients to everyone else on the message.
///
/// `fallback_signature` applies only when the draft expresses no preference. That precedence lives
/// here rather than in each caller, so there is no way to build a message that bypasses it — which
/// is exactly how a signature ends up on the message twice.
pub fn build_message(
    draft: &Draft,
    from_name: &str,
    from_addr: &str,
    fallback_signature: &str,
) -> Result<Vec<u8>> {
    let signature = draft.signature.as_deref().unwrap_or(fallback_signature);
    let body = if signature.trim().is_empty() {
        draft.body_text.clone()
    } else {
        format!("{}\r\n\r\n-- \r\n{}", draft.body_text, signature)
    };

    // The HTML half gets the same signature, marked up so it does not arrive as one run-on line.
    let html_body = draft.body_html.as_ref().map(|html| {
        if signature.trim().is_empty() {
            html.clone()
        } else {
            format!(
                "{html}<div style=\"margin-top:14px\">--<br>{}</div>",
                signature.replace('\n', "<br>")
            )
        }
    });

    // Checked here as well as before queueing, because this is the one function every send,
    // scheduled send and retry goes through.
    let invalid = draft.invalid_recipients();
    if !invalid.is_empty() {
        return Err(Error::Parse(format!(
            "not an email address: {}",
            invalid.join(", ")
        )));
    }

    let mut builder = MessageBuilder::new()
        .from((from_name, from_addr))
        .message_id(message_id(from_addr))
        .subject(draft.subject.clone());

    // `multipart/alternative` when both exist: the text part is what a client without HTML shows,
    // and its absence is something several spam filters score against.
    let body_part = || match &html_body {
        Some(html) => MimePart::new(
            "multipart/alternative",
            vec![
                MimePart::new("text/plain", body.clone()),
                MimePart::new("text/html", html.clone()),
            ],
        ),
        None => MimePart::new("text/plain", body.clone()),
    };

    if draft.attachments.is_empty() {
        builder = match &html_body {
            Some(html) => builder.text_body(body.clone()).html_body(html.clone()),
            None => builder.text_body(body.clone()),
        };
    } else {
        // multipart/mixed: the body first, then each file.
        let mut parts = vec![body_part()];
        let mut total = 0u64;
        for att in &draft.attachments {
            let data = std::fs::read(&att.path)
                .map_err(|e| Error::Parse(format!("could not read {}: {e}", att.path)))?;
            total += data.len() as u64;
            if total > MAX_ATTACHMENT_TOTAL {
                return Err(Error::Parse(format!(
                    "attachments total more than {} MB, which most servers refuse",
                    MAX_ATTACHMENT_TOTAL / (1024 * 1024)
                )));
            }
            let name = signet_mime::safe_filename(&att.filename);
            parts.push(MimePart::new(guess_mime(&name), data).attachment(name));
        }
        builder = builder.body(MimePart::new("multipart/mixed", parts));
    }

    if draft.to.iter().any(|e| !e.trim().is_empty()) {
        builder = builder.to(address_list(&draft.to));
    }
    if draft.cc.iter().any(|e| !e.trim().is_empty()) {
        builder = builder.cc(address_list(&draft.cc));
    }
    if let Some(parent) = &draft.in_reply_to {
        builder = builder.in_reply_to(parent.clone());
    }
    if !draft.references.is_empty() {
        builder = builder.references(draft.references.clone());
    }

    builder
        .write_to_vec()
        .map_err(|e| Error::Parse(format!("could not build message: {e}")))
}

/// Queue a draft for sending and write its raw form to disk.
pub fn queue(
    store: &Store,
    draft: &Draft,
    from_name: &str,
    from_addr: &str,
    signature: &str,
) -> Result<i64> {
    let raw = build_message(draft, from_name, from_addr, signature)?;
    let dir = store.blob_dir(draft.account_id).join("outbox");
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}.eml", signet_store::now_utc()));
    std::fs::write(&path, &raw)?;

    // The draft is stored with the row for the same reason as in `queue_at`: without it a failed
    // send could not be retried by the timer, and was parked as failed instead.
    let draft_json = serde_json::to_string(draft).unwrap_or_default();
    Ok(store.queue_scheduled(
        draft.account_id,
        &draft.envelope_recipients().join(","),
        &draft.subject,
        &path.to_string_lossy(),
        &draft_json,
        0,
    )?)
}

/// Queue a message to go out later.
///
/// The draft is stored alongside the built message because sending is not the whole job: the copy
/// filed in Sent and the removal of the original draft both need the structured draft, and after a
/// restart there is nothing else to rebuild it from.
pub fn queue_at(
    store: &Store,
    draft: &Draft,
    from_name: &str,
    from_addr: &str,
    signature: &str,
    scheduled_utc: i64,
) -> Result<i64> {
    let raw = build_message(draft, from_name, from_addr, signature)?;
    let dir = store.blob_dir(draft.account_id).join("outbox");
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}-{}.eml", signet_store::now_utc(), scheduled_utc));
    std::fs::write(&path, &raw)?;

    let draft_json = serde_json::to_string(draft).unwrap_or_default();
    Ok(store.queue_scheduled(
        draft.account_id,
        &draft.envelope_recipients().join(","),
        &draft.subject,
        &path.to_string_lossy(),
        &draft_json,
        scheduled_utc,
    )?)
}

/// What became of a send attempt that did not fail outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// Accepted by the server and filed in Sent.
    Delivered,
    /// Another caller had already claimed this row; nothing was done.
    AlreadyTaken,
}

/// Send one queued message, then file a copy in Sent.
///
/// Sends the message exactly as it was built when queued. Rebuilding here gave every retry a new
/// Message-ID and re-read attachments from wherever the user had since moved them.
pub async fn send_now(
    store: &Store,
    outbox_id: i64,
    draft: &Draft,
    password: &str,
) -> Result<Sent> {
    let account = store.get_account(draft.account_id)?;
    if !store.claim_outbox(outbox_id)? {
        return Ok(Sent::AlreadyTaken);
    }

    let stored = store
        .outbox_raw_path(outbox_id)?
        .and_then(|path| std::fs::read(path).ok());
    let raw = match stored {
        Some(raw) => raw,
        None => match build_message(draft, &account.display_name, &account.email, &account.signature) {
            Ok(raw) => raw,
            Err(e) => {
                store.set_outbox_state(outbox_id, "failed", Some(&e.to_string()))?;
                return Err(e);
            }
        },
    };

    let config = crate::smtp_config_for(&account);
    let recipients = draft.envelope_recipients();

    match signet_net::smtp::send_raw(&config, password, &account.email, &recipients, &raw).await {
        Ok(()) => {
            store.set_outbox_state(outbox_id, "sent", None)?;
            file_in_sent(store, &account, draft, &raw)?;
            // A sent message is no longer a draft. Done after filing, so a failure here leaves a
            // duplicate rather than losing the message entirely.
            if let Some(draft_id) = draft.draft_message_id {
                if let Err(e) = store.purge_messages(&[draft_id]) {
                    tracing::warn!(error = %e, "sent message but could not remove the draft");
                }
            }
            Ok(Sent::Delivered)
        }
        Err(e) => {
            let transient = e.is_transient();
            store.set_outbox_state(
                outbox_id,
                if transient { "queued" } else { "failed" },
                Some(&e.to_string()),
            )?;
            Err(Error::Net(e))
        }
    }
}

/// Write the sent copy into the local Sent folder.
///
/// SMTP sends and nothing stores, so if we do not do this the message vanishes.
fn file_in_sent(
    store: &Store,
    account: &signet_store::Account,
    draft: &Draft,
    raw: &[u8],
) -> Result<()> {
    let folder_id = store.folder_id(account.id, FolderKind::Sent)?;
    let dir = store.blob_dir(account.id).join("raw");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("sent-{}.eml", signet_store::now_utc()));
    std::fs::write(&path, raw)?;

    let now = signet_store::now_utc();
    let mut recipients = Vec::new();
    for (kind, list) in [("to", &draft.to), ("cc", &draft.cc), ("bcc", &draft.bcc)] {
        for (name, addr) in list.iter().filter_map(|e| parse_mailbox(e)) {
            recipients.push(Recipient {
                kind: kind.to_string(),
                name: name.unwrap_or_default(),
                addr,
            });
        }
    }

    let mut stored_attachments = Vec::new();
    for att in &draft.attachments {
        // Copy into our blob store: the source file is the user's and may move or be deleted,
        // and a Sent copy that cannot show its own attachments is not a copy.
        let name = signet_mime::safe_filename(&att.filename);
        let dest = dir.join(format!("sent-{}-{name}", signet_store::now_utc()));
        if std::fs::copy(&att.path, &dest).is_ok() {
            let size = std::fs::metadata(&dest)
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            stored_attachments.push(NewAttachment {
                filename: name.clone(),
                mime_type: guess_mime(&name).to_string(),
                size_bytes: size,
                content_id: None,
                blob_path: dest.to_string_lossy().to_string(),
                is_inline: false,
            });
        }
    }

    // The id the recipients got. It is what team state keys on, and what lets the server's copy
    // of this message be recognised as this one rather than downloaded as a second.
    let parsed = signet_mime::parse(raw);
    store.insert_message(&NewMessage {
        account_id: account.id,
        folder_id,
        rfc822_message_id: parsed.as_ref().and_then(|p| p.message_id.clone()),
        in_reply_to: draft.in_reply_to.clone(),
        thread_id: draft
            .references
            .first()
            .cloned()
            .or_else(|| draft.in_reply_to.clone())
            .unwrap_or_else(|| signet_mime::normalize_subject(&draft.subject)),
        from_name: account.display_name.clone(),
        from_addr: account.email.clone(),
        subject: draft.subject.clone(),
        date_utc: now,
        received_utc: now,
        has_attachments: !stored_attachments.is_empty(),
        has_remote_content: false,
        size_bytes: raw.len() as i64,
        preview_text: draft.body_text.chars().take(200).collect(),
        body_text: draft.body_text.clone(),
        // The rich version, so the Sent copy reads the way the recipient's does. Storing `None`
        // meant every sent message was displayed from its plain-text alternative, which is the
        // flattened fallback rather than what was actually written.
        body_html: draft
            .body_html
            .as_ref()
            .map(|html| signet_mime::sanitize(html, true).html),
        raw_path: path.to_string_lossy().to_string(),
        recipients,
        attachments: stored_attachments,
    })?;

    // IMAP: SMTP delivers but stores nothing, so the server's Sent folder would never show what
    // this computer sent and the rest of the team could not see it. Gmail is the exception: it
    // files SMTP mail in Sent itself, and uploading as well would make two copies.
    if account.protocol == signet_store::Protocol::Imap && !files_sent_mail_itself(&account.smtp_host) {
        if let Some(sent_path) = store.remote_path_of(folder_id)? {
            store.queue_imap_append(account.id, &sent_path, &path.to_string_lossy())?;
        }
    }

    Ok(())
}

/// Servers that put SMTP-submitted mail into Sent on their own.
fn files_sent_mail_itself(smtp_host: &str) -> bool {
    let host = smtp_host.to_ascii_lowercase();
    host.ends_with("gmail.com") || host.ends_with("googlemail.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft {
            account_id: 1,
            to: vec!["a@example.com".into()],
            cc: vec!["c@example.com".into()],
            bcc: vec!["secret@example.com".into()],
            subject: "Subject line".into(),
            body_text: "Body".into(),
            ..Default::default()
        }
    }

    #[test]
    fn bcc_reaches_the_envelope_but_never_the_headers() {
        let d = draft();
        let raw = build_message(&d, "Dev", "dev@example.com", "").unwrap();
        let text = String::from_utf8_lossy(&raw);

        // The whole point of Bcc.
        assert!(
            !text.to_lowercase().contains("secret@example.com"),
            "bcc recipient disclosed in headers:\n{text}"
        );
        assert!(text.contains("a@example.com"));
        assert!(text.contains("c@example.com"));

        // ...but it must still be delivered.
        assert!(
            d.envelope_recipients()
                .contains(&"secret@example.com".to_string())
        );
        assert_eq!(d.envelope_recipients().len(), 3);
    }

    #[test]
    fn recipients_typed_or_pasted_the_way_outlook_writes_them_are_understood() {
        for (entry, name, addr) in [
            ("a@x.org", None, "a@x.org"),
            ("  <a@x.org> ", None, "a@x.org"),
            ("Janice Carroll <janice@x.org>", Some("Janice Carroll"), "janice@x.org"),
            ("\"Doe, John\" <j@x.org>", Some("Doe, John"), "j@x.org"),
        ] {
            assert_eq!(
                parse_mailbox(entry),
                Some((name.map(str::to_string), addr.to_string())),
                "{entry}"
            );
        }
        for bad in ["Doe", "bad address", "a@x", "a@@x.org", "@x.org", "a b@x.org"] {
            assert_eq!(parse_mailbox(bad), None, "{bad} accepted");
        }
    }

    #[test]
    fn a_named_recipient_gets_a_valid_header_and_a_bare_envelope_address() {
        // The real failure: `To: <Janice Carroll <janice@x.org>>` and an RCPT the server refused.
        let d = Draft {
            account_id: 1,
            to: vec!["Janice Carroll <janice@x.org>".into()],
            cc: vec!["b@x.org".into(), "B@X.ORG".into()],
            subject: "s".into(),
            body_text: "b".into(),
            ..Default::default()
        };
        let raw = String::from_utf8(build_message(&d, "Dev", "dev@example.com", "").unwrap()).unwrap();
        let head = raw.split("\r\n\r\n").next().unwrap();
        assert!(head.contains("To: \"Janice Carroll\" <janice@x.org>"), "{head}");
        assert!(!head.contains("<Janice Carroll <"), "{head}");
        assert_eq!(d.envelope_recipients(), vec!["b@x.org", "janice@x.org"]);
    }

    #[test]
    fn a_mistyped_address_is_refused_by_name_before_anything_is_sent() {
        let d = Draft {
            to: vec!["ok@x.org".into(), "bad address".into()],
            ..Default::default()
        };
        assert_eq!(d.invalid_recipients(), vec!["bad address"]);
        let err = build_message(&d, "Dev", "dev@example.com", "").unwrap_err().to_string();
        assert!(err.contains("bad address"), "{err}");
    }

    #[test]
    fn the_message_id_is_on_the_senders_domain_not_the_computers_name() {
        let raw = String::from_utf8(build_message(&draft(), "Dev", "dev@example.com", "").unwrap()).unwrap();
        let id = raw.lines().find(|l| l.starts_with("Message-ID:")).unwrap();
        assert!(id.ends_with("@example.com>"), "{id}");
    }

    #[test]
    fn envelope_recipients_are_deduplicated_and_trimmed() {
        let d = Draft {
            to: vec!["  a@x.test ".into(), "a@x.test".into()],
            cc: vec!["".into(), "  ".into()],
            ..Default::default()
        };
        assert_eq!(d.envelope_recipients(), vec!["a@x.test".to_string()]);
    }

    #[test]
    fn signature_is_appended_with_the_standard_separator() {
        let raw = build_message(&draft(), "Dev", "dev@example.com", "Example Co").unwrap();
        // Asserted on the decoded body, not the wire bytes: the separator is "-- " with a
        // trailing space, which MIME requires be quoted-printable encoded as "--=20".
        let parsed = signet_mime::parse(&raw).unwrap();
        assert!(parsed.body_text.contains("Example Co"));
        assert!(
            parsed.body_text.contains("-- "),
            "missing the RFC 3676 signature separator: {:?}",
            parsed.body_text
        );
        assert!(parsed.body_text.contains("Body"));
    }

    #[test]
    fn reply_headers_preserve_the_thread() {
        let d = Draft {
            in_reply_to: Some("<parent@example.com>".into()),
            references: vec!["<root@example.com>".into(), "<parent@example.com>".into()],
            ..draft()
        };
        let raw = build_message(&d, "Dev", "dev@example.com", "").unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("In-Reply-To"));
        assert!(text.contains("References"));
        assert!(text.contains("root@example.com"));
    }

    #[test]
    fn built_messages_round_trip_through_the_parser() {
        let raw = build_message(&draft(), "Dev", "dev@example.com", "").unwrap();
        let parsed = signet_mime::parse(&raw).expect("our own output must be parseable");
        assert_eq!(parsed.subject, "Subject line");
        assert_eq!(parsed.from.unwrap().addr, "dev@example.com");
        assert!(parsed.body_text.contains("Body"));
    }
}
