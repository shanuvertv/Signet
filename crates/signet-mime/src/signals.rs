//! Facts extracted from a message that a spam filter can reason about.
//!
//! Deliberately *facts*, not judgements: this module says "DKIM failed" or "this link's text says
//! one domain and its href goes to another", and `signet-sync::junk` decides what that is worth.
//! Keeping the two apart means the scoring can be retuned without touching any parsing, and the
//! parsing can be tested without asserting on scores.

use std::collections::HashSet;

/// How the receiving server judged the sender's authentication.
///
/// We cannot verify SPF/DKIM/DMARC ourselves — that requires DNS at delivery time, which the
/// server already did. What we can do is read the verdict it recorded, which is why the
/// `Authentication-Results` header exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthVerdict {
    Pass,
    Fail,
    /// The server said nothing, or nothing we recognised. Common on older setups, so it must not
    /// be treated as failure.
    #[default]
    Unknown,
}

impl AuthVerdict {
    fn from_token(value: &str) -> Self {
        // e.g. "dkim=pass header.d=example.com" or "spf=fail (sender IP is 1.2.3.4)"
        let v = value.trim_start();
        if v.starts_with("pass") {
            AuthVerdict::Pass
        } else if v.starts_with("fail")
            || v.starts_with("softfail")
            || v.starts_with("permerror")
            || v.starts_with("none")
        {
            // `none` means the domain publishes no policy — not a failure, but not a pass either.
            if v.starts_with("none") {
                AuthVerdict::Unknown
            } else {
                AuthVerdict::Fail
            }
        } else {
            AuthVerdict::Unknown
        }
    }
}

/// One link found in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub href: String,
    /// The text the reader actually sees.
    pub text: String,
    pub host: String,
}

/// Everything the filter looks at beyond the words themselves.
#[derive(Debug, Clone, Default)]
pub struct Signals {
    pub spf: AuthVerdict,
    pub dkim: AuthVerdict,
    pub dmarc: AuthVerdict,

    /// `Reply-To` pointing at a different domain from `From`. Legitimate for mailing lists, and a
    /// staple of business-email-compromise attempts.
    pub reply_to_mismatch: bool,
    /// `Return-Path` domain differs from `From` domain.
    pub return_path_mismatch: bool,

    /// The message announces itself as bulk mail.
    pub bulk: bool,
    pub has_list_unsubscribe: bool,

    pub links: Vec<Link>,
    /// A link whose visible text names one domain while its href goes to another — the classic
    /// phishing tell.
    pub deceptive_links: Vec<Link>,
    /// Links written as a bare IP address rather than a hostname.
    pub ip_literal_links: usize,
    /// Distinct hosts linked to. A newsletter has a few; a spam run often has many.
    pub distinct_link_hosts: usize,

    /// Attachment extensions Windows will execute, or that carry macros.
    pub risky_attachments: Vec<String>,

    /// No `Message-ID` at all. Nearly every real sender emits one.
    pub missing_message_id: bool,
    /// `From` header with no parseable address.
    pub missing_from: bool,
}

/// Extensions Windows executes, or that carry macros.
const RISKY_EXTENSIONS: [&str; 26] = [
    "exe", "scr", "com", "pif", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf",
    "wsh", "msi", "msp", "hta", "cpl", "jar", "reg", "lnk", "docm", "xlsm", "pptm", "dotm", "iso",
];

/// URL shorteners hide the real destination, which is the point of using one in a phishing mail.
const SHORTENERS: [&str; 10] = [
    "bit.ly",
    "tinyurl.com",
    "goo.gl",
    "t.co",
    "ow.ly",
    "is.gd",
    "buff.ly",
    "rebrand.ly",
    "cutt.ly",
    "shorturl.at",
];

/// Extract every signal from a parsed message plus its raw headers.
pub fn extract(parsed: &crate::ParsedMessage, raw_headers: &str) -> Signals {
    let mut s = Signals {
        missing_message_id: parsed.message_id.as_deref().unwrap_or("").trim().is_empty(),
        missing_from: parsed
            .from
            .as_ref()
            .map(|f| f.addr.trim().is_empty())
            .unwrap_or(true),
        ..Default::default()
    };

    let from_domain = parsed
        .from
        .as_ref()
        .and_then(|f| domain_of(&f.addr))
        .unwrap_or_default();

    // Only the first `Authentication-Results` header may be believed. Each server that handles a
    // message *prepends* its own, so the topmost one is the verdict of the last server to touch it
    // -- our own -- and everything below it arrived with the message. A sender is free to write
    // `Authentication-Results: mx.example.com; spf=pass; dkim=pass; dmarc=pass` into what they
    // submit, and taking the last one seen would let them forge their own pass, collect the
    // authenticated bonus, and suppress the DMARC-failure escalation at the same time. RFC 8601
    // §5 says as much: a consumer must ignore any instance it cannot attribute to a trusted MTA.
    let mut auth_taken = false;

    for line in unfold(raw_headers) {
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => continue,
        };

        match name.as_str() {
            "authentication-results" if !auth_taken => {
                auth_taken = true;
                let lower = value.to_ascii_lowercase();
                if let Some(v) = after(&lower, "spf=") {
                    s.spf = AuthVerdict::from_token(&v);
                }
                if let Some(v) = after(&lower, "dkim=") {
                    s.dkim = AuthVerdict::from_token(&v);
                }
                if let Some(v) = after(&lower, "dmarc=") {
                    s.dmarc = AuthVerdict::from_token(&v);
                }
            }
            "list-unsubscribe" => s.has_list_unsubscribe = true,
            "precedence" => {
                let v = value.to_ascii_lowercase();
                if v.contains("bulk") || v.contains("list") || v.contains("junk") {
                    s.bulk = true;
                }
            }
            "reply-to" => {
                if let Some(d) = domain_of(&value) {
                    if !from_domain.is_empty() && !d.eq_ignore_ascii_case(&from_domain) {
                        s.reply_to_mismatch = true;
                    }
                }
            }
            "return-path" => {
                if let Some(d) = domain_of(&value) {
                    if !from_domain.is_empty() && !d.eq_ignore_ascii_case(&from_domain) {
                        s.return_path_mismatch = true;
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(html) = &parsed.body_html {
        s.links = extract_links(html);
    }

    let mut hosts = HashSet::new();
    for link in &s.links {
        if link.host.is_empty() {
            continue;
        }
        hosts.insert(link.host.to_ascii_lowercase());
        if is_ip_literal(&link.host) {
            s.ip_literal_links += 1;
        }
        if let Some(claimed) = domain_in_text(&link.text) {
            // The text names a domain; if the href goes somewhere else, that is deception rather
            // than a style choice.
            if !same_site(&claimed, &link.host) {
                s.deceptive_links.push(link.clone());
            }
        }
    }
    s.distinct_link_hosts = hosts.len();

    for att in &parsed.attachments {
        let ext = att
            .filename
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if RISKY_EXTENSIONS.contains(&ext.as_str()) {
            s.risky_attachments.push(att.filename.clone());
        }
    }

    s
}

/// Whether a host is a known URL shortener.
pub fn is_shortener(host: &str) -> bool {
    let h = host.trim_start_matches("www.").to_ascii_lowercase();
    SHORTENERS.contains(&h.as_str())
}

/// Header lines, with RFC 5322 folding undone.
fn unfold(headers: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in headers.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // A continuation belongs to the header above it.
            if let Some(last) = out.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
                continue;
            }
        }
        out.push(line.to_string());
    }
    out
}

fn after(haystack: &str, needle: &str) -> Option<String> {
    haystack
        .find(needle)
        .map(|i| haystack[i + needle.len()..].to_string())
}

/// The domain of an address, however it is wrapped.
pub fn domain_of(value: &str) -> Option<String> {
    let cleaned = value.trim().trim_matches(['<', '>']);
    let addr = cleaned
        .rsplit(['<', ' '])
        .next()
        .unwrap_or(cleaned)
        .trim_matches(['<', '>']);
    addr.rsplit_once('@')
        .map(|(_, d)| d.trim().trim_matches('>').to_ascii_lowercase())
        .filter(|d| !d.is_empty() && d.contains('.'))
}

fn is_ip_literal(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']);
    h.parse::<std::net::IpAddr>().is_ok()
}

/// The registrable part of a host, crudely: the last two labels.
///
/// Not a public-suffix implementation — `example.co.uk` reduces to `co.uk` — but it is only used
/// to decide whether two hosts look like the same site, where over-matching is the safe error.
fn registrable(host: &str) -> String {
    let parts: Vec<&str> = host.trim_start_matches("www.").split('.').collect();
    if parts.len() <= 2 {
        return parts.join(".");
    }
    parts[parts.len() - 2..].join(".")
}

fn same_site(a: &str, b: &str) -> bool {
    registrable(&a.to_ascii_lowercase()).eq_ignore_ascii_case(&registrable(&b.to_ascii_lowercase()))
}

/// A domain named in visible link text, if it names one at all.
fn domain_in_text(text: &str) -> Option<String> {
    let t = text.trim().trim_end_matches(['.', ',', ')']);
    let candidate = t
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', '?', ' '])
        .next()
        .unwrap_or("");

    // Must look like a hostname, and not be an address or a sentence.
    if candidate.contains('@') || candidate.contains(' ') || !candidate.contains('.') {
        return None;
    }
    let labels: Vec<&str> = candidate.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return None;
    }
    // A trailing label that is not alphabetic is not a TLD.
    if !labels
        .last()
        .unwrap()
        .chars()
        .all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    Some(candidate.to_ascii_lowercase())
}

/// Pull `<a href>` links and their visible text out of an HTML body.
fn extract_links(html: &str) -> Vec<Link> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut at = 0usize;

    while let Some(rel) = lower[at..].find("<a ") {
        let start = at + rel;
        let Some(tag_end_rel) = lower[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end_rel;
        let tag = &html[start..tag_end];

        let href = attr_value(tag, "href").unwrap_or_default();
        let text_end = lower[tag_end..]
            .find("</a>")
            .map(|i| tag_end + i)
            .unwrap_or(lower.len());
        let text = strip_tags(&html[tag_end + 1..text_end.min(html.len())]);

        if href.starts_with("http") {
            let host = host_of(&href);
            out.push(Link { href, text, host });
        }

        at = (tag_end + 1).min(lower.len());
        if at >= lower.len() {
            break;
        }
    }
    out
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let key = format!("{name}=");
    let i = lower.find(&key)? + key.len();
    let rest = &tag[i..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)? + 1;
        Some(rest[1..end].to_string())
    } else {
        Some(
            rest.split([' ', '>'])
                .next()
                .unwrap_or_default()
                .to_string(),
        )
    }
}

/// Host part of an absolute URL.
pub fn host_of(url: &str) -> String {
    let after_scheme = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        // Strip any userinfo: `https://paypal.com@evil.test/` is a real trick.
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(html: Option<&str>) -> crate::ParsedMessage {
        crate::ParsedMessage {
            message_id: Some("<a@b.test>".into()),
            from: Some(crate::Addr {
                name: "Sender".into(),
                addr: "sender@example.com".into(),
            }),
            body_html: html.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn authentication_results_are_read_for_all_three_mechanisms() {
        let headers = "Authentication-Results: mx.test; spf=pass smtp.mailfrom=example.com; \
                       dkim=fail header.d=example.com; dmarc=fail";
        let s = extract(&parsed(None), headers);
        assert_eq!(s.spf, AuthVerdict::Pass);
        assert_eq!(s.dkim, AuthVerdict::Fail);
        assert_eq!(s.dmarc, AuthVerdict::Fail);
    }

    #[test]
    fn a_missing_header_is_unknown_not_failure() {
        // Plenty of legitimate servers add nothing. Treating silence as failure would file half
        // the mailbox as junk.
        let s = extract(&parsed(None), "Subject: hello");
        assert_eq!(s.spf, AuthVerdict::Unknown);
        assert_eq!(s.dkim, AuthVerdict::Unknown);
        assert_eq!(s.dmarc, AuthVerdict::Unknown);
    }

    #[test]
    fn a_domain_publishing_no_policy_is_unknown_not_failure() {
        let s = extract(&parsed(None), "Authentication-Results: mx.test; dmarc=none");
        assert_eq!(s.dmarc, AuthVerdict::Unknown);
    }

    #[test]
    fn folded_headers_are_read_as_one_line() {
        let headers = "Authentication-Results: mx.test;\r\n\tspf=fail smtp.mailfrom=example.com";
        let s = extract(&parsed(None), headers);
        assert_eq!(s.spf, AuthVerdict::Fail, "folding hid the verdict");
    }

    #[test]
    fn a_reply_to_on_another_domain_is_noticed() {
        let s = extract(&parsed(None), "Reply-To: accounts@totally-different.test");
        assert!(s.reply_to_mismatch);

        let same = extract(&parsed(None), "Reply-To: someone@example.com");
        assert!(!same.reply_to_mismatch);
    }

    #[test]
    fn bulk_markers_are_detected() {
        let s = extract(
            &parsed(None),
            "Precedence: bulk\r\nList-Unsubscribe: <mailto:x@y.test>",
        );
        assert!(s.bulk);
        assert!(s.has_list_unsubscribe);
    }

    #[test]
    fn a_link_whose_text_names_a_different_domain_is_deceptive() {
        // The single most reliable phishing tell.
        let html = r#"<a href="https://evil.test/login">https://paypal.com/login</a>"#;
        let s = extract(&parsed(Some(html)), "");
        assert_eq!(s.deceptive_links.len(), 1);
        assert_eq!(s.deceptive_links[0].host, "evil.test");
    }

    #[test]
    fn an_honest_link_is_not_flagged() {
        let html = r#"<a href="https://www.example.com/x">example.com</a>"#;
        let s = extract(&parsed(Some(html)), "");
        assert!(s.deceptive_links.is_empty(), "{:?}", s.deceptive_links);
    }

    #[test]
    fn link_text_that_is_not_a_domain_is_not_treated_as_a_claim() {
        // "Click here" claims nothing, so it cannot contradict the href.
        let html = r#"<a href="https://example.com/x">Click here to continue</a>"#;
        let s = extract(&parsed(Some(html)), "");
        assert!(s.deceptive_links.is_empty());
    }

    #[test]
    fn userinfo_in_a_url_does_not_disguise_the_real_host() {
        // `https://paypal.com@evil.test/` goes to evil.test, and reading the host naively gets it
        // exactly backwards.
        assert_eq!(host_of("https://paypal.com@evil.test/login"), "evil.test");
    }

    #[test]
    fn bare_ip_links_are_counted() {
        let html = r#"<a href="http://203.0.113.9/pay">pay now</a>"#;
        let s = extract(&parsed(Some(html)), "");
        assert_eq!(s.ip_literal_links, 1);
    }

    #[test]
    fn distinct_hosts_are_counted_once_each() {
        let html = r#"<a href="https://a.test/1">one</a><a href="https://a.test/2">two</a>
                      <a href="https://b.test/3">three</a>"#;
        let s = extract(&parsed(Some(html)), "");
        assert_eq!(s.links.len(), 3);
        assert_eq!(s.distinct_link_hosts, 2);
    }

    #[test]
    fn shorteners_are_recognised() {
        assert!(is_shortener("bit.ly"));
        assert!(is_shortener("www.tinyurl.com"));
        assert!(!is_shortener("example.com"));
    }

    #[test]
    fn risky_attachments_are_listed_by_name() {
        let mut p = parsed(None);
        p.attachments = vec![
            crate::ParsedAttachment {
                filename: "invoice.pdf".into(),
                mime_type: "application/pdf".into(),
                content_id: None,
                is_inline: false,
                data: vec![],
            },
            crate::ParsedAttachment {
                filename: "payload.exe".into(),
                mime_type: "application/octet-stream".into(),
                content_id: None,
                is_inline: false,
                data: vec![],
            },
        ];
        let s = extract(&p, "");
        assert_eq!(s.risky_attachments, vec!["payload.exe".to_string()]);
    }

    #[test]
    fn a_message_with_no_message_id_or_sender_is_flagged() {
        let p = crate::ParsedMessage::default();
        let s = extract(&p, "");
        assert!(s.missing_message_id);
        assert!(s.missing_from);
    }

    #[test]
    fn subdomains_count_as_the_same_site() {
        // `mail.example.com` linking to `example.com` is not deception.
        let html = r#"<a href="https://mail.example.com/x">example.com</a>"#;
        let s = extract(&parsed(Some(html)), "");
        assert!(s.deceptive_links.is_empty());
    }

    #[test]
    fn a_forged_authentication_results_header_cannot_override_the_real_one() {
        // Each server prepends its own, so ours is at the top and anything below it arrived with
        // the message. A sender who writes their own "pass" must not be believed -- doing so would
        // hand them the authenticated bonus and switch off the DMARC-failure escalation at once.
        let raw = concat!(
            "Authentication-Results: mx.example.com; spf=fail; dkim=fail; dmarc=fail\r\n",
            "Authentication-Results: mx.example.com; spf=pass; dkim=pass; dmarc=pass\r\n",
            "From: Someone <a@sender.test>\r\n",
            "Subject: hello\r\n",
            "\r\n",
        );
        let parsed = crate::parse(raw.as_bytes()).expect("parse");
        let s = extract(&parsed, &parsed.raw_headers);

        assert_eq!(s.dmarc, AuthVerdict::Fail, "a forged pass was believed");
        assert_eq!(s.dkim, AuthVerdict::Fail);
        assert_eq!(s.spf, AuthVerdict::Fail);
    }

    #[test]
    fn the_topmost_authentication_results_is_the_one_that_counts() {
        // The same rule the other way round: a genuine pass at the top stands, even when an older
        // hop below it recorded something else.
        let raw = concat!(
            "Authentication-Results: mx.example.com; spf=pass; dkim=pass; dmarc=pass\r\n",
            "Authentication-Results: relay.example.test; spf=fail; dkim=fail; dmarc=fail\r\n",
            "From: Someone <a@sender.test>\r\n",
            "Subject: hello\r\n",
            "\r\n",
        );
        let parsed = crate::parse(raw.as_bytes()).expect("parse");
        let s = extract(&parsed, &parsed.raw_headers);

        assert_eq!(s.dmarc, AuthVerdict::Pass);
        assert_eq!(s.spf, AuthVerdict::Pass);
    }
}
