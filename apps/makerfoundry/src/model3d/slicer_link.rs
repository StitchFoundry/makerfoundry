//! "Open in OrcaSlicer" links (issue #101, slice 1): the `orcaslicer://` address that makes the
//! slicer download a file and open it. Only builds the string; no route or button uses it yet, and
//! nothing is fetched here.
//!
//! The format, from OrcaSlicer's own source (`Downloader.cpp`, `Utils.hpp`):
//! `orcaslicer://open?file=<URL-encoded https link>`. The slicer URL-decodes the part after
//! `file=` once and downloads it, so the link is encoded exactly once, like JavaScript's
//! `encodeURIComponent`. Needs OrcaSlicer 2.1.0 or newer.
//! <https://github.com/OrcaSlicer/OrcaSlicer/blob/main/src/slic3r/GUI/Downloader.cpp>

/// The longest download link we will wrap; longer ones are refused rather than truncated.
pub const MAX_LINK_BYTES: usize = 2048;

/// The oldest OrcaSlicer that opens such links (generic URL download, 2024-05-22).
pub const MIN_ORCA_VERSION: &str = "2.1.0";

/// `text` as a query value: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )` is `%XX`
/// (what `encodeURIComponent` leaves alone).
fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            for nibble in [b >> 4, b & 15] {
                out.push(char::from_digit(u32::from(nibble), 16).map_or('0', |c| c.to_ascii_uppercase()));
            }
        }
    }
    out
}

/// The `orcaslicer://` link that opens `download_url`, or `None` if that isn't a plain https link
/// (https only, a host, no spaces or control characters, at most `MAX_LINK_BYTES`).
pub fn orcaslicer_open(download_url: &str) -> Option<String> {
    let rest = download_url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let plain = download_url.len() <= MAX_LINK_BYTES
        && !host.is_empty()
        && !host.starts_with(['@', ':'])
        && download_url.bytes().all(|b| b.is_ascii_graphic());
    plain.then(|| format!("orcaslicer://open?file={}", encode_component(download_url)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_a_link_encoded_once() {
        assert_eq!(
            orcaslicer_open("https://makerfoundry.app/3d/dl/abc.3mf?t=1&s=a+b").as_deref(),
            Some("orcaslicer://open?file=https%3A%2F%2Fmakerfoundry.app%2F3d%2Fdl%2Fabc.3mf%3Ft%3D1%26s%3Da%2Bb")
        );
    }

    #[test]
    fn leaves_what_encode_uri_component_leaves() {
        assert_eq!(encode_component("a-b_c.d!e~f*g'h(i)9"), "a-b_c.d!e~f*g'h(i)9");
        assert_eq!(encode_component("%/ é"), "%25%2F%20%C3%A9");
    }

    #[test]
    fn the_encoding_decodes_back_to_the_link() {
        let url = "https://example.com/a/b.3mf?x=%41&y=1#z";
        let link = orcaslicer_open(url).unwrap();
        let value = link.strip_prefix("orcaslicer://open?file=").unwrap();
        let mut bytes = Vec::new();
        let raw = value.as_bytes();
        let mut i = 0;
        while let Some(&b) = raw.get(i) {
            if b == b'%' {
                let hex = std::str::from_utf8(raw.get(i + 1..i + 3).unwrap()).unwrap();
                bytes.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                bytes.push(b);
                i += 1;
            }
        }
        assert_eq!(String::from_utf8(bytes).unwrap(), url);
    }

    #[test]
    fn refuses_anything_but_a_plain_https_link() {
        for bad in [
            "",
            "http://example.com/a.3mf",
            "ftp://example.com/a.3mf",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "HTTPS://example.com/a.3mf",
            "https://",
            "https:///a.3mf",
            "https://?x=1",
            "https://:443/a",
            "https://example.com/a b.3mf",
            "https://example.com/a\n.3mf",
            "https://example.com/é.3mf",
        ] {
            assert_eq!(orcaslicer_open(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn refuses_a_link_that_is_too_long() {
        let ok = format!("https://example.com/{}", "a".repeat(MAX_LINK_BYTES - 20));
        assert_eq!(ok.len(), MAX_LINK_BYTES);
        assert!(orcaslicer_open(&ok).is_some());
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_BYTES));
        assert_eq!(orcaslicer_open(&long), None);
    }
}
