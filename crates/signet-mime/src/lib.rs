//! MIME parsing, and sanitization of untrusted HTML.
//!
//! Everything that arrives here is attacker-controlled. `mail-parser` handles the malformed,
//! mislabelled and mis-encoded reality of real mail; [`sanitize`] handles the fact that an HTML
//! body is hostile input we are about to render.

pub mod sanitize;
pub mod signals;
pub mod signature;

use mail_parser::{Address, MessageParser, MimeHeaders};

pub use sanitize::{SanitizedHtml, sanitize};
pub use signals::{AuthVerdict, Link, Signals};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    pub name: String,
    pub addr: String,
}

#[derive(Debug, Clone)]
pub struct ParsedAttachment {
    pub filename: String,
    pub mime_type: String,
    pub content_id: Option<String>,
    pub is_inline: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedMessage {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Option<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub bcc: Vec<Addr>,
    pub subject: String,
    /// Unix seconds. Falls back to the receive time when the header is missing or unparseable —
    /// a message with no usable Date must not sort to 1970.
    pub date_utc: Option<i64>,
    pub body_text: String,
    pub body_html: Option<String>,
    pub attachments: Vec<ParsedAttachment>,
    /// 0 low, 1 normal, 2 high, from the sender's own headers. `None` when they said nothing.
    pub priority: Option<i64>,
    /// The raw header block, kept so the spam signals can read headers we do not model.
    pub raw_headers: String,
}

impl ParsedMessage {
    /// First ~200 characters of the text body, collapsed to one line, for the list pane.
    pub fn preview(&self) -> String {
        let source = if self.body_text.trim().is_empty() {
            self.body_html
                .as_deref()
                .map(sanitize::html_to_text)
                .unwrap_or_default()
        } else {
            self.body_text.clone()
        };
        let collapsed = source.split_whitespace().collect::<Vec<_>>().join(" ");
        collapsed.chars().take(200).collect()
    }

    /// The thread key, following the JWZ approach as far as headers allow.
    ///
    /// `References` wins because it survives clients that drop `In-Reply-To`. With neither, we
    /// fall back to the normalized subject, which is wrong occasionally but groups the common
    /// "Re: Re: Fwd:" case that users actually notice.
    pub fn thread_id(&self) -> String {
        if let Some(root) = self.references.first() {
            return root.clone();
        }
        if let Some(parent) = &self.in_reply_to {
            return parent.clone();
        }
        if let Some(id) = &self.message_id {
            return id.clone();
        }
        normalize_subject(&self.subject)
    }
}

/// Strip reply and forward prefixes so a thread survives them.
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let trimmed = ["re:", "fw:", "fwd:", "aw:", "sv:", "vs:", "antwort:"]
            .iter()
            .find_map(|p| lower.starts_with(p).then(|| s[p.len()..].trim_start()));
        match trimmed {
            Some(rest) => s = rest,
            None => break,
        }
    }
    s.to_ascii_lowercase()
}

/// Parse a raw RFC822 message.
///
/// Returns `None` only when the input is not a message at all. Partial or malformed messages are
/// parsed as far as possible, because a mail client that refuses to show broken mail is worse
/// than one that shows it imperfectly.
pub fn parse(raw: &[u8]) -> Option<ParsedMessage> {
    let msg = MessageParser::default().parse(raw)?;

    let mut out = ParsedMessage {
        message_id: msg.message_id().map(str::to_string),
        in_reply_to: msg
            .in_reply_to()
            .as_text_list()
            .and_then(|l| l.first().map(|s| s.to_string())),
        references: msg
            .references()
            .as_text_list()
            .map(|l| l.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default(),
        from: msg.from().and_then(first_addr),
        to: msg.to().map(all_addrs).unwrap_or_default(),
        cc: msg.cc().map(all_addrs).unwrap_or_default(),
        bcc: msg.bcc().map(all_addrs).unwrap_or_default(),
        subject: msg.subject().unwrap_or_default().to_string(),
        date_utc: msg.date().map(|d| d.to_timestamp()),
        body_text: msg.body_text(0).map(|c| c.to_string()).unwrap_or_default(),
        body_html: msg.body_html(0).map(|c| c.to_string()),
        attachments: Vec::new(),
        priority: read_priority(&msg),
        raw_headers: header_block(raw),
    };

    for part in msg.attachments() {
        let filename = part
            .attachment_name()
            .map(str::to_string)
            .unwrap_or_else(|| "attachment".to_string());
        let mime_type = part
            .content_type()
            .map(|c| match c.subtype() {
                Some(sub) => format!("{}/{}", c.ctype(), sub),
                None => c.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let content_id = part.content_id().map(str::to_string);
        let is_inline = part
            .content_disposition()
            .is_some_and(|d| d.ctype().eq_ignore_ascii_case("inline"))
            || content_id.is_some();

        out.attachments.push(ParsedAttachment {
            filename: safe_filename(&filename),
            mime_type,
            content_id,
            is_inline,
            data: part.contents().to_vec(),
        });
    }

    // A message with only an HTML body still needs text for search and the preview line.
    if out.body_text.trim().is_empty() {
        if let Some(html) = &out.body_html {
            out.body_text = sanitize::html_to_text(html);
        }
    }

    Some(out)
}

/// Reduce an attachment filename to something safe to write to disk.
///
/// Attachment names are attacker-controlled: `../../etc/passwd`, `C:\Windows\...`, embedded NULs
/// and trailing dots have all been used to escape a download directory. We keep the final
/// component only, and strip anything that could be a separator or a device name.
pub fn safe_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_matches('.');

    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '\0'..='\x1f' | '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\' => '_',
            other => other,
        })
        .collect();

    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        return "attachment".to_string();
    }

    // Windows refuses these names regardless of extension.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = cleaned.split('.').next().unwrap_or(&cleaned).to_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return format!("_{cleaned}");
    }

    cleaned.chars().take(200).collect()
}

/// Importance as the sender declared it.
///
/// Three competing conventions are in the wild and clients disagree about which they emit, so we
/// read all of them: `X-Priority: 1..5` (1 highest), `Importance: high|normal|low`, and
/// `X-MSMail-Priority`. Only the extremes are worth acting on — "normal" is the default anyway.
fn read_priority(msg: &mail_parser::Message<'_>) -> Option<i64> {
    let header = |name: &str| -> Option<String> {
        msg.header(name)
            .and_then(|h| h.as_text())
            .map(|v| v.trim().to_lowercase())
    };

    if let Some(v) = header("X-Priority").or_else(|| header("X-MSMail-Priority")) {
        // "1 (Highest)" and "1" both appear.
        let leading: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
        match leading.parse::<i64>() {
            Ok(1) | Ok(2) => return Some(2),
            Ok(4) | Ok(5) => return Some(0),
            Ok(3) => return Some(1),
            _ => {}
        }
        if v.contains("high") {
            return Some(2);
        }
        if v.contains("low") {
            return Some(0);
        }
    }

    if let Some(v) = header("Importance") {
        if v.starts_with("high") {
            return Some(2);
        }
        if v.starts_with("low") {
            return Some(0);
        }
        if v.starts_with("normal") {
            return Some(1);
        }
    }

    None
}

/// Everything before the blank line that separates headers from body.
fn header_block(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    // Either CRLF CRLF or LF LF, whichever comes first — real mail uses both.
    let end = [text.find("\r\n\r\n"), text.find("\n\n")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(text.len());
    text[..end].to_string()
}

fn first_addr(a: &Address<'_>) -> Option<Addr> {
    all_addrs(a).into_iter().next()
}

fn all_addrs(a: &Address<'_>) -> Vec<Addr> {
    a.iter()
        .filter_map(|m| {
            m.address().map(|addr| Addr {
                name: m.name().unwrap_or_default().to_string(),
                addr: addr.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>, carol@example.com\r\n\
Subject: Hello there\r\n\
Message-ID: <one@example.com>\r\n\
Date: Tue, 1 Apr 2025 10:00:00 +0000\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Body text here.\r\n";

    #[test]
    fn parses_addresses_subject_and_body() {
        let m = parse(SIMPLE).unwrap();
        assert_eq!(m.from.as_ref().unwrap().addr, "alice@example.com");
        assert_eq!(m.from.as_ref().unwrap().name, "Alice");
        assert_eq!(m.to.len(), 2);
        assert_eq!(m.to[1].addr, "carol@example.com");
        assert_eq!(m.subject, "Hello there");
        assert!(m.body_text.contains("Body text here."));
        assert_eq!(m.message_id.as_deref(), Some("one@example.com"));
        assert!(m.date_utc.is_some());
    }

    #[test]
    fn decodes_encoded_word_headers() {
        let raw = b"From: =?utf-8?B?SMOpbMOobmU=?= <h@example.com>\r\n\
Subject: =?utf-8?Q?Lumi=C3=A8re?=\r\n\
\r\n\
body\r\n";
        let m = parse(raw).unwrap();
        assert_eq!(m.subject, "Lumière");
        assert_eq!(m.from.unwrap().name, "Hélène");
    }

    #[test]
    fn html_only_messages_still_get_searchable_text() {
        let raw = b"Subject: HTML only\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>Hello <b>world</b></p></body></html>\r\n";
        let m = parse(raw).unwrap();
        assert!(m.body_html.is_some());
        // Without this, an HTML-only message would be invisible to search.
        assert!(m.body_text.contains("Hello"), "got {:?}", m.body_text);
        assert!(m.body_text.contains("world"));
        assert!(!m.body_text.contains("<b>"));
    }

    #[test]
    fn extracts_attachments_with_their_metadata() {
        let raw = b"Subject: With attachment\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"B\"\r\n\
\r\n\
--B\r\n\
Content-Type: text/plain\r\n\
\r\n\
See attached.\r\n\
--B\r\n\
Content-Type: text/csv; name=\"data.csv\"\r\n\
Content-Disposition: attachment; filename=\"data.csv\"\r\n\
\r\n\
a,b,c\r\n\
--B--\r\n";
        let m = parse(raw).unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].filename, "data.csv");
        assert_eq!(m.attachments[0].mime_type, "text/csv");
        assert!(m.body_text.contains("See attached."));
    }

    #[test]
    fn attachment_filenames_cannot_escape_the_download_directory() {
        // Every one of these has been used in the wild to write outside the target directory.
        assert_eq!(safe_filename("../../etc/passwd"), "passwd");
        assert_eq!(
            safe_filename("..\\..\\windows\\system32\\evil.dll"),
            "evil.dll"
        );
        assert_eq!(safe_filename("/absolute/path.txt"), "path.txt");
        assert_eq!(safe_filename("C:\\Users\\x\\report.pdf"), "report.pdf");
        assert!(!safe_filename("bad\0name.txt").contains('\0'));
        assert_eq!(safe_filename(""), "attachment");
        assert_eq!(safe_filename("..."), "attachment");
        // Reserved Windows device names are refused by the filesystem whatever the extension.
        assert_eq!(safe_filename("CON.txt"), "_CON.txt");
        assert_eq!(safe_filename("nul"), "_nul");
    }

    #[test]
    fn threading_prefers_references_then_in_reply_to() {
        let mut m = ParsedMessage {
            message_id: Some("child@x".into()),
            ..Default::default()
        };
        assert_eq!(m.thread_id(), "child@x");

        m.in_reply_to = Some("parent@x".into());
        assert_eq!(m.thread_id(), "parent@x");

        m.references = vec!["root@x".into(), "parent@x".into()];
        assert_eq!(m.thread_id(), "root@x", "References must win");
    }

    #[test]
    fn subject_normalization_survives_stacked_prefixes() {
        assert_eq!(normalize_subject("Re: Fwd: RE: Budget"), "budget");
        assert_eq!(normalize_subject("  AW: Angebot "), "angebot");
        assert_eq!(normalize_subject("Plain"), "plain");
    }

    #[test]
    fn preview_collapses_whitespace_and_truncates() {
        let m = ParsedMessage {
            body_text: format!("line one\r\n\r\n   line two{}", "x".repeat(500)),
            ..Default::default()
        };
        let p = m.preview();
        assert!(p.starts_with("line one line two"));
        assert_eq!(p.chars().count(), 200);
    }

    #[test]
    fn priority_headers_are_read_in_all_three_conventions() {
        let with = |header: &str| {
            let raw = format!("Subject: x\r\n{header}\r\n\r\nbody\r\n");
            parse(raw.as_bytes()).unwrap().priority
        };
        assert_eq!(with("X-Priority: 1 (Highest)"), Some(2));
        assert_eq!(with("X-Priority: 5"), Some(0));
        assert_eq!(with("X-Priority: 3"), Some(1));
        assert_eq!(with("Importance: High"), Some(2));
        assert_eq!(with("Importance: low"), Some(0));
        assert_eq!(with("X-MSMail-Priority: High"), Some(2));
        // Silence is not the same as "normal": we keep our own default rather than overriding.
        assert_eq!(with("X-Other: nothing"), None);
    }

    #[test]
    fn a_message_with_no_headers_at_all_still_parses() {
        // Servers do deliver these. Refusing to show them is worse than showing them badly.
        let m = parse(b"just a body with no headers").unwrap();
        assert!(m.from.is_none());
        assert_eq!(m.subject, "");
    }
}
