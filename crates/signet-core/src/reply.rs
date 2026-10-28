//! Building the body of a reply or a forward.
//!
//! Almost all business mail is HTML, and quoting it by flattening it to text and prefixing every
//! line with `>` destroys it: tables collapse, links come apart across line breaks, and the result
//! looks nothing like what the recipient sent. Outlook, Gmail and Apple Mail all do the same thing
//! instead — keep the original as HTML, and put a small header block above it saying where it came
//! from. That is what this produces.
//!
//! A plain-text alternative is still generated alongside, because a message with no `text/plain`
//! part is treated as suspicious by some filters and is unreadable on the handful of clients that
//! do not render HTML.

use signet_store::MessageDetail;

/// The two bodies of a reply: what goes in `text/plain` and what goes in `text/html`.
#[derive(Debug, Clone)]
pub struct ReplyBody {
    pub text: String,
    pub html: String,
}

/// Escape text for insertion into HTML.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Turn a plain-text body into HTML, preserving its line structure.
///
/// Used when the original had no HTML part at all. `white-space: pre-wrap` rather than `<br>` per
/// line so that runs of spaces in a quoted table survive.
fn text_to_html(text: &str) -> String {
    format!(
        r#"<div style="white-space:pre-wrap;font-family:Consolas,monospace;font-size:13px">{}</div>"#,
        esc(text)
    )
}

/// The `From: / Sent: / To: / Subject:` block Outlook puts above a quoted message.
///
/// Deliberately the same shape and wording. Everyone in this trade reads these every day, and a
/// reply that quotes differently from every other reply in the thread looks broken even when it is
/// not.
fn quote_header(m: &MessageDetail, sent: &str) -> String {
    quote_header_labelled(m, sent, None)
}

/// The header rows, optionally introduced by a line of their own.
fn quote_header_labelled(m: &MessageDetail, sent: &str, intro: Option<&str>) -> String {
    let from = if m.from_name.trim().is_empty() {
        esc(&m.from_addr)
    } else {
        format!("{} &lt;{}&gt;", esc(&m.from_name), esc(&m.from_addr))
    };

    let to = m
        .recipients
        .iter()
        .filter(|r| r.kind == "to")
        .map(|r| esc(&r.addr))
        .collect::<Vec<_>>()
        .join("; ");
    let cc = m
        .recipients
        .iter()
        .filter(|r| r.kind == "cc")
        .map(|r| esc(&r.addr))
        .collect::<Vec<_>>()
        .join("; ");

    let mut rows = String::new();
    if let Some(intro) = intro {
        rows.push_str(&format!("<b>{}</b><br>", esc(intro)));
    }
    rows.push_str(&format!(
        "<b>From:</b> {from}<br>\
         <b>Sent:</b> {}<br>\
         <b>To:</b> {to}<br>",
        esc(sent)
    ));
    if !cc.is_empty() {
        rows.push_str(&format!("<b>Cc:</b> {cc}<br>"));
    }
    rows.push_str(&format!("<b>Subject:</b> {}<br>", esc(&m.subject)));
    rows
}

/// Build both bodies for a reply.
///
/// `intro` is the empty space the user types into — it comes first, because a reply is read from
/// the top and nobody should have to scroll past the thing they are replying to in order to write.
pub fn reply_body(m: &MessageDetail, sent: &str) -> ReplyBody {
    let original = m
        .body_html
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| text_to_html(&m.body_text));

    // Outlook's own shape, deliberately: a hairline rule, the header block, then the original at
    // full width. No `blockquote` and no left border — that is Gmail's convention, and mixing the
    // two in one thread is what makes a reply look out of place next to everyone else's.
    let html = format!(
        r#"<div><br></div>
<div><br></div>
<div class="signet-quote">
<div style="border:none;border-top:1pt solid #E1E1E1;padding:3pt 0 0 0">
<p style="margin:0;font-family:Calibri,sans-serif;font-size:11pt">{header}</p>
</div>
<p style="margin:0">&nbsp;</p>
{original}
</div>"#,
        header = quote_header(m, sent),
        original = original,
    );

    let quoted_text = m
        .body_text
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    let who = if m.from_name.trim().is_empty() {
        &m.from_addr
    } else {
        &m.from_name
    };
    let text = format!("\r\n\r\nOn {sent}, {who} wrote:\r\n{quoted_text}");

    ReplyBody { text, html }
}

/// Build both bodies for a forward.
///
/// The difference from a reply is only the wording of the separator: a forward is presented as
/// somebody else's message being passed on, not as a conversation being continued.
pub fn forward_body(m: &MessageDetail, sent: &str) -> ReplyBody {
    let original = m
        .body_html
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| text_to_html(&m.body_text));

    let html = format!(
        r#"<div><br></div>
<div><br></div>
<div class="signet-quote">
<div style="border:none;border-top:1pt solid #E1E1E1;padding:3pt 0 0 0">
<p style="margin:0;font-family:Calibri,sans-serif;font-size:11pt">{header}</p>
</div>
<p style="margin:0">&nbsp;</p>
{original}
</div>"#,
        header = quote_header_labelled(m, sent, Some("---------- Forwarded message ----------")),
        original = original,
    );

    let to = m
        .recipients
        .iter()
        .filter(|r| r.kind == "to")
        .map(|r| r.addr.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!(
        "\r\n\r\n---------- Forwarded message ----------\r\nFrom: {} <{}>\r\nSent: {sent}\r\n\
         To: {to}\r\nSubject: {}\r\n\r\n{}",
        m.from_name, m.from_addr, m.subject, m.body_text
    );

    ReplyBody { text, html }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signet_store::{MessageDetail, Recipient};

    fn message(html: Option<&str>) -> MessageDetail {
        MessageDetail {
            id: 1,
            account_id: 1,
            folder_id: 1,
            rfc822_message_id: Some("abc@x".into()),
            from_name: "Dana Whitfield".into(),
            from_addr: "dana.whitfield@alliance.example".into(),
            subject: "Associate membership invoice".into(),
            date_utc: 1_700_000_000,
            is_read: true,
            is_flagged: false,
            is_answered: false,
            body_html: html.map(str::to_string),
            body_text: "Dear Sam,\n\nThe membership dues are assessed quarterly.".into(),
            has_remote_content: false,
            recipients: vec![
                Recipient {
                    kind: "to".into(),
                    name: String::new(),
                    addr: "dev@example.com".into(),
                },
                Recipient {
                    kind: "cc".into(),
                    name: String::new(),
                    addr: "accounts@example.com".into(),
                },
            ],
            attachments: vec![],
            size_bytes: 100,
        }
    }

    #[test]
    fn the_original_html_is_kept_rather_than_flattened() {
        // The whole point. Flattening to text and prefixing `>` is what made replies look broken
        // next to Outlook's.
        let body = reply_body(
            &message(Some("<p>Dear <b>Sam</b>,</p>")),
            "3 September 2026",
        );
        assert!(
            body.html.contains("<p>Dear <b>Sam</b>,</p>"),
            "{}",
            body.html
        );
        assert!(
            !body.html.contains("&gt; Dear"),
            "the body was quoted as text"
        );
    }

    #[test]
    fn the_quote_header_reads_the_way_outlook_writes_it() {
        let body = reply_body(
            &message(Some("<p>hi</p>")),
            "Thursday, 3 September 2026 4:38 PM",
        );
        for expected in [
            "<b>From:</b>",
            "<b>Sent:</b>",
            "<b>To:</b>",
            "<b>Subject:</b>",
        ] {
            assert!(body.html.contains(expected), "missing {expected}");
        }
        assert!(
            body.html
                .contains("Dana Whitfield &lt;dana.whitfield@alliance.example&gt;")
        );
        assert!(body.html.contains("Thursday, 3 September 2026 4:38 PM"));
    }

    #[test]
    fn cc_appears_only_when_there_was_one() {
        let with_cc = reply_body(&message(Some("<p>hi</p>")), "now");
        assert!(with_cc.html.contains("<b>Cc:</b>"));

        let mut m = message(Some("<p>hi</p>"));
        m.recipients.retain(|r| r.kind == "to");
        assert!(!reply_body(&m, "now").html.contains("<b>Cc:</b>"));
    }

    #[test]
    fn a_plain_text_original_still_becomes_readable_html() {
        // Not every message has an HTML part, and those must not arrive as one run-on line.
        let body = reply_body(&message(None), "now");
        assert!(body.html.contains("white-space:pre-wrap"), "{}", body.html);
        assert!(body.html.contains("Dear Sam,"));
    }

    #[test]
    fn the_users_reply_goes_above_the_quote() {
        // A reply is read from the top. The empty div is where the composer puts the cursor.
        let body = reply_body(&message(Some("<p>hi</p>")), "now");
        let empty = body.html.find("<div><br></div>").expect("no space to write in");
        let quote = body.html.find("border-top").expect("no quote block");
        assert!(
            empty < quote,
            "the quote comes before the space to write in"
        );
    }

    #[test]
    fn a_plain_text_alternative_is_always_produced() {
        // A message with no text/plain part is treated as suspicious by some filters.
        let body = reply_body(&message(Some("<p>hi</p>")), "now");
        assert!(body.text.contains("> Dear Sam,"));
        assert!(body.text.contains("Dana Whitfield wrote:"));
    }

    #[test]
    fn a_subject_containing_markup_cannot_break_out_of_the_header() {
        // Subjects come from strangers, and this one is inserted into HTML we build.
        let mut m = message(Some("<p>hi</p>"));
        m.subject = "<script>alert(1)</script>".into();
        let body = reply_body(&m, "now");
        assert!(!body.html.contains("<script>"), "{}", body.html);
        assert!(body.html.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_quote_uses_outlooks_shape_not_gmails() {
        // Outlook puts a hairline rule, the header, then the original at full width. Gmail indents
        // behind a left border. Mixing the two in one thread is what makes a reply look wrong.
        let body = reply_body(&message(Some("<p>hi</p>")), "now");
        assert!(
            body.html.contains("border-top:1pt solid #E1E1E1"),
            "no Outlook rule above the quote: {}",
            body.html
        );
        assert!(
            !body.html.contains("blockquote"),
            "the original was indented behind a border: {}",
            body.html
        );
        assert!(
            !body.html.contains("border-left"),
            "the original was indented behind a border: {}",
            body.html
        );
    }

    #[test]
    fn the_quote_block_is_marked_so_the_composer_can_find_it() {
        // The signature is placed above it and the plain-text half is quoted from it.
        assert!(reply_body(&message(None), "now").html.contains("signet-quote"));
        assert!(
            forward_body(&message(None), "now")
                .html
                .contains("signet-quote")
        );
    }

    #[test]
    fn a_forward_says_it_is_a_forward() {
        let body = forward_body(&message(Some("<p>hi</p>")), "now");
        assert!(body.html.contains("Forwarded message"));
        assert!(body.text.contains("Forwarded message"));
        // The label sits in the header block itself, the way Outlook writes it.
        assert!(
            body.html
                .contains("<b>---------- Forwarded message ----------</b>")
        );
    }
}
