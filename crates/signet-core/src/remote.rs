//! Remote images, fetched here rather than by the webview.
//!
//! When the user clicks "Show images", the obvious thing is to leave the `https:` URLs in the HTML
//! and let the reading pane load them. That fails for a large share of real mail: CDNs such as
//! claude.ai's send `Cross-Origin-Resource-Policy: same-origin`, and a browser engine refuses to
//! draw such an image inside any other page — every logo and icon arrives broken. Outlook is not a
//! browser and never sees the header.
//!
//! So the images are downloaded here and put into the HTML as `data:` URIs, the same way
//! [`crate::inline`] handles embedded `cid:` parts. It only ever happens after the user has asked
//! to show images for this one message.

use std::time::Duration;

use base64::Engine;

/// Largest single image accepted. Mail images are logos and banners; anything bigger is not.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// Cap on distinct images fetched for one message, so a hostile body cannot make us fetch forever.
const MAX_IMAGES: usize = 60;

/// Replace every remote `<img src="http(s)://…">` in sanitized HTML with a `data:` URI.
///
/// Expects the sanitizer's output, where attributes are always double-quoted and `&` is written
/// `&amp;`. An image that fails to download keeps its URL, so it shows broken exactly as before
/// rather than vanishing.
pub fn inline_remote_images(html: &str) -> String {
    let urls = remote_srcs(html);
    if urls.is_empty() {
        return html.to_string();
    }

    let agent = agent();
    // Fetched in parallel: a newsletter has dozens of images and one slow host must not make the
    // user wait for the sum of them.
    let fetched: Vec<(String, Option<String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = urls
            .iter()
            .map(|raw| {
                let agent = &agent;
                s.spawn(move || (raw.clone(), fetch_data_uri(agent, &raw.replace("&amp;", "&"))))
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let mut out = html.to_string();
    for (raw, data_uri) in fetched {
        if let Some(data_uri) = data_uri {
            out = out.replace(&format!("src=\"{raw}\""), &format!("src=\"{data_uri}\""));
        }
    }
    out
}

/// The distinct remote `src` values of `<img>` elements, as written in the HTML.
fn remote_srcs(html: &str) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for (i, _) in html.match_indices("<img ") {
        let tag_end = html[i..].find('>').map_or(html.len(), |e| i + e);
        let tag = &html[i..tag_end];
        let Some(start) = tag.find("src=\"").map(|p| p + 5) else {
            continue;
        };
        let Some(len) = tag[start..].find('"') else {
            continue;
        };
        let url = &tag[start..start + len];
        let lower = url.to_ascii_lowercase();
        if (lower.starts_with("https://") || lower.starts_with("http://"))
            && !urls.iter().any(|u| u == url)
        {
            urls.push(url.to_string());
        }
        if urls.len() >= MAX_IMAGES {
            break;
        }
    }
    urls
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        // Some image hosts answer a client with no user agent with an error page.
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Signet")
        .build()
        .new_agent()
}

/// Download one image. `None` for anything that is not an image or is too large.
fn fetch_data_uri(agent: &ureq::Agent, url: &str) -> Option<String> {
    let mut resp = agent.get(url).call().ok()?;
    let mime = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())?;
    // Only images go in: a `data:text/html` in an `img` is harmless, but there is no reason to
    // carry anything else, and SVG can hold script even though the sandbox would not run it.
    if !mime.starts_with("image/") || mime.contains("svg") {
        return None;
    }
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_IMAGE_BYTES)
        .read_to_vec()
        .ok()?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{mime};base64,{encoded}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_image_sources_are_found_once_each() {
        let html = r#"<p>x</p><img alt="" src="https://a.test/1.png" width="20">
            <img src="https://a.test/1.png"><img src="cid:logo"><img src="data:image/png;base64,AA">
            <img src="http://b.test/p.gif?u=1&amp;v=2">"#;
        assert_eq!(
            remote_srcs(html),
            vec!["https://a.test/1.png", "http://b.test/p.gif?u=1&amp;v=2"]
        );
    }

    #[test]
    fn a_body_with_no_remote_images_is_returned_untouched() {
        let html = r#"<img src="cid:logo"><a href="https://x.test">link</a>"#;
        assert_eq!(inline_remote_images(html), html);
    }

    #[test]
    fn an_image_that_cannot_be_fetched_keeps_its_url() {
        // `.invalid` never resolves, so this runs offline.
        let html = r#"<img src="https://nothing.invalid/x.png">"#;
        assert_eq!(inline_remote_images(html), html);
    }
}
