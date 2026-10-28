//! Resolving `cid:` image references to the attachment bytes they name.
//!
//! HTML mail embeds its own images by reference: `<img src="cid:logo@example.com">` points at a
//! MIME part carried in the same message. The sanitizer keeps those URLs — they are not remote and
//! leak nothing — but a browser has no idea what `cid:` means, and the reading pane renders into a
//! `sandbox=""` iframe that cannot fetch anything anyway. Left alone they are permanently broken
//! images, which is what signatures, logos and most newsletters are made of.
//!
//! So they are rewritten to `data:` URIs at **read** time, not at ingest: mail already downloaded
//! is fixed without anyone having to re-sync, and the stored HTML stays as the sanitizer produced
//! it.

use base64::Engine;
use signet_store::{MessageDetail, Store};

/// Inline images larger than this are left unresolved.
///
/// Base64 inflates by a third and the whole body is handed to the WebView as one string, so a
/// 40 MB "inline" image would freeze the reading pane. Real inline images — logos, signatures —
/// are a few tens of kilobytes.
const MAX_INLINE_BYTES: u64 = 2 * 1024 * 1024;

/// Replace `cid:` references in the body with `data:` URIs built from the message's own parts.
///
/// Anything that cannot be resolved is left as it was: a broken image is better than mangled
/// markup, and it is the honest signal that something is missing.
pub fn resolve_cid_images(store: &Store, detail: &mut MessageDetail) {
    let Some(html) = detail.body_html.as_ref() else {
        return;
    };
    if !html.to_ascii_lowercase().contains("cid:") {
        return;
    }

    let mut out = html.clone();
    let mut resolved = 0usize;

    for attachment in &detail.attachments {
        let Some(cid) = attachment.content_id.as_deref() else {
            continue;
        };
        // Content-IDs travel with angle brackets in the header and without them in `src`, and
        // different clients disagree about which they store.
        let key = cid.trim().trim_start_matches('<').trim_end_matches('>');
        if key.is_empty() {
            continue;
        }

        if attachment.size_bytes as u64 > MAX_INLINE_BYTES {
            tracing::debug!(
                cid = key,
                size = attachment.size_bytes,
                "inline image too large to embed; leaving the reference unresolved"
            );
            continue;
        }

        let Ok((_, path)) = store.attachment_path(attachment.id) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            tracing::warn!(path = %path.display(), "inline image blob missing");
            continue;
        };

        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let data_uri = format!("data:{};base64,{}", attachment.mime_type, encoded);

        let before = out.len();
        out = replace_cid(&out, key, &data_uri);
        if out.len() != before {
            resolved += 1;
        }
    }

    if resolved > 0 {
        tracing::debug!(resolved, message = detail.id, "inlined cid images");
        detail.body_html = Some(out);
    }
}

/// Replace every `cid:<key>` occurrence, however the message quoted it.
///
/// Case-insensitive on the `cid:` scheme (RFC 2392 says the scheme is case-insensitive) but exact
/// on the identifier, because Content-IDs are case-sensitive and two parts in one message can
/// legitimately differ only by case.
fn replace_cid(html: &str, key: &str, data_uri: &str) -> String {
    let needle = format!("cid:{key}");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    loop {
        // Find the next `cid:` in any casing.
        let Some(pos) = find_ignore_ascii_case(rest, "cid:") else {
            out.push_str(rest);
            return out;
        };

        let (head, tail) = rest.split_at(pos);
        out.push_str(head);

        // The identifier itself must match exactly, and must end at a quote or delimiter so that
        // `cid:logo` does not also match `cid:logo2`.
        let candidate = &tail[..needle.len().min(tail.len())];
        let ends_cleanly = tail
            .as_bytes()
            .get(needle.len())
            .is_none_or(|b| !matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'@' | b'+'));

        if candidate.len() == needle.len()
            && candidate[4..].eq(&needle[4..])
            && candidate[..4].eq_ignore_ascii_case("cid:")
            && ends_cleanly
        {
            out.push_str(data_uri);
            rest = &tail[needle.len()..];
        } else {
            // Not our reference; copy the scheme and carry on so a later match is still found.
            out.push_str(&tail[..4]);
            rest = &tail[4..];
        }
    }
}

fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    let n = needle.len();
    haystack
        .as_bytes()
        .windows(n)
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
        .filter(|&i| haystack.is_char_boundary(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_reference_is_replaced() {
        let html = r#"<img src="cid:logo@x.test">"#;
        let out = replace_cid(html, "logo@x.test", "data:image/png;base64,AAA");
        assert_eq!(out, r#"<img src="data:image/png;base64,AAA">"#);
    }

    #[test]
    fn the_scheme_is_case_insensitive_but_the_id_is_not() {
        // RFC 2392: the scheme is case-insensitive. Content-IDs are not.
        assert!(replace_cid(r#"<img src="CID:logo">"#, "logo", "D").contains('D'));
        assert!(!replace_cid(r#"<img src="cid:LOGO">"#, "logo", "D").contains('D'));
    }

    #[test]
    fn a_longer_id_is_not_matched_by_a_shorter_one() {
        // `cid:logo` must not swallow `cid:logo2`, or the wrong image appears.
        let html = r#"<img src="cid:logo2">"#;
        assert_eq!(replace_cid(html, "logo", "DATA"), html);
    }

    #[test]
    fn several_references_are_all_replaced() {
        let html = r#"<img src="cid:a"><p>x</p><img src="cid:a">"#;
        let out = replace_cid(html, "a", "D");
        assert_eq!(out.matches('D').count(), 2);
        assert!(!out.contains("cid:a"));
    }

    #[test]
    fn unrelated_references_survive_untouched() {
        let html = r#"<img src="cid:one"><img src="cid:two">"#;
        let out = replace_cid(html, "one", "D");
        assert!(
            out.contains("cid:two"),
            "the other reference was eaten: {out}"
        );
        assert!(!out.contains("cid:one"));
    }

    #[test]
    fn text_that_merely_mentions_cid_is_left_alone() {
        let html = "<p>see the cid: scheme in RFC 2392</p>";
        assert_eq!(replace_cid(html, "logo", "D"), html);
    }

    #[test]
    fn a_reference_with_no_matching_part_is_left_as_it_was() {
        // A broken image is honest; mangled markup is not.
        let html = r#"<img src="cid:missing@x.test">"#;
        assert_eq!(replace_cid(html, "other@x.test", "D"), html);
    }

    #[test]
    fn multibyte_content_before_a_reference_does_not_panic() {
        let html = "<p>Lumière — résumé</p><img src=\"cid:logo\">";
        let out = replace_cid(html, "logo", "D");
        assert!(out.contains('D'));
        assert!(out.contains("Lumière"));
    }
}
