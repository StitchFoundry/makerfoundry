//! The shared page layout and HTML helpers.

/// The site's mark, drawn in code: the three crafts the site serves on one tile. A needle and
/// thread (embroidery), a laser head and its beam (laser cutting), and a print nozzle over printed
/// layers (3D printing). Also served as `/favicon.svg`.
pub const MARK_SVG: &str = r##"<svg class="mark" viewBox="0 0 64 64" width="32" height="32" aria-hidden="true">
<rect width="64" height="64" rx="14" fill="#b4462a"/>
<g transform="translate(32 31) scale(0.86) translate(-32 -32)">
<path d="M11 46 L19 13" stroke="#fbfaf7" stroke-width="3.6" stroke-linecap="round"/>
<ellipse cx="19.6" cy="11" rx="2" ry="3.2" transform="rotate(14 19.6 11)" fill="none" stroke="#fbfaf7" stroke-width="1.8"/>
<path d="M19.6 11 C26 16, 12 24, 16 32 S 22 42, 12 50" fill="none" stroke="#ffd166" stroke-width="2.4" stroke-dasharray="3.4 2.4" stroke-linecap="round"/>
<rect x="26" y="10" width="13" height="9" rx="2.2" fill="#fbfaf7"/><path d="M29.5 19 H35.5 L34 23 H31 Z" fill="#fbfaf7"/>
<path d="M32.5 23 V44" stroke="#ffd166" stroke-width="3" stroke-linecap="round"/><circle cx="32.5" cy="46" r="3" fill="#fff3c4"/>
<rect x="43" y="10" width="13" height="8" rx="2.2" fill="#fbfaf7"/><path d="M46.5 18 H52.5 L49.5 24 Z" fill="#fbfaf7"/>
<rect x="44" y="43" width="11" height="4" rx="1.2" fill="#fbfaf7"/><rect x="45" y="37.5" width="9" height="4" rx="1.2" fill="#fbfaf7"/><rect x="46" y="32" width="7" height="4" rx="1.2" fill="#fbfaf7"/>
</g>
</svg>"##;

/// `text` made safe to put in HTML, in an element or a quoted attribute.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Wraps a page's body in the shared layout. `title` and `body` are HTML: anything a person typed
/// goes through [`escape`] before it gets here. `signed_in` says whether the header offers the
/// account or a way to sign in.
pub fn page(title: &str, body: &str, signed_in: bool) -> String {
    page_with(title, body, signed_in, None)
}

/// What a link to a public page shows where it's shared (a chat, a social site): the standard
/// Open Graph tags. Plain text; escaped here.
pub struct Preview<'a> {
    pub title: &'a str,
    pub description: &'a str,
    /// The page's address, in full.
    pub url: &'a str,
    /// A picture's address, in full.
    pub image: Option<&'a str>,
}

/// `text` on one line, at most `max` characters.
pub fn summary(text: &str, max: usize) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        format!(
            "{}…",
            one.chars().take(max.saturating_sub(1)).collect::<String>().trim_end()
        )
    }
}

/// [`page`], with a shared link's preview.
pub fn page_with(title: &str, body: &str, signed_in: bool, preview: Option<&Preview>) -> String {
    let head = match preview {
        None => r#"<meta name="description" content="Machine-embroidery designs, laser-cut files and 3D-print models, shared and sold by makers.">"#.to_string(),
        Some(p) => {
            let image = match p.image {
                Some(i) => format!(
                    r#"<meta property="og:image" content="{}"><meta name="twitter:card" content="summary_large_image">"#,
                    escape(i)
                ),
                None => r#"<meta name="twitter:card" content="summary">"#.to_string(),
            };
            format!(
                r#"<meta name="description" content="{d}">
<link rel="canonical" href="{u}">
<meta property="og:site_name" content="MakerFoundry"><meta property="og:type" content="website">
<meta property="og:title" content="{t}"><meta property="og:description" content="{d}"><meta property="og:url" content="{u}">{image}"#,
                t = escape(p.title),
                d = escape(p.description),
                u = escape(p.url),
            )
        }
    };
    let nav = if signed_in {
        r#"<a href="/browse">Browse</a><a class="upload" href="/designs/new">Upload</a><a href="/account">Account</a>"#
    } else {
        r#"<a href="/browse">Browse</a><a class="upload" href="/signup">Upload</a><a href="/signin">Sign in</a>"#
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
{head}
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/static/site.css">
</head>
<body>
<header class="top"><a class="brand" href="/">{MARK_SVG}<span>MakerFoundry</span></a><form class="hsearch" role="search" method="get" action="/browse"><input type="search" name="q" placeholder="Search designs" aria-label="Search designs" maxlength="200"></form><nav>{nav}</nav></header>
<main>
{body}
</main>
<footer class="bottom"><p>MakerFoundry is made by <a href="https://sitesentrix.com">SiteSentrix</a>,
the makers of <a href="https://stitchfoundry.app">StitchFoundry</a>.</p>
<p class="links"><a href="/terms">Terms</a> <a href="/privacy">Privacy</a> <a href="/seller-terms">Seller Terms</a> <a href="/dmca">Copyright</a> <a href="https://www.facebook.com/makerfoundryapp" rel="noopener">Facebook</a> <a href="https://www.youtube.com/@MakerFoundryApp" rel="noopener">YouTube</a> <a href="https://buymeacoffee.com/bwattendorf" rel="noopener">Buy us a coffee</a></p>
<p class="report">Found a problem, or have an idea? <a class="button secondary" href="mailto:bwattendorf@gmail.com?subject=%5BMakerFoundry%5D%20Bug%20report%3A%20&amp;body=What%20happened%3A%0A%0A%0ASteps%20to%20get%20there%3A%0A1.%20%0A2.%20%0A%0AWhat%20you%20expected%20instead%3A%0A%0A%0AThe%20page%20%28its%20address%29%3A%0A">Report a bug</a> <a class="button secondary" href="mailto:bwattendorf@gmail.com?subject=%5BMakerFoundry%5D%20Feature%20request%3A%20&amp;body=What%20you%20would%20like%20MakerFoundry%20to%20do%3A%0A%0A%0AWhy%20it%20would%20help%20you%3A%0A">Suggest a feature</a></p></footer>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_makes_text_safe_in_elements_and_attributes() {
        assert_eq!(
            escape(r#"<a href="x">Tom & Jerry's</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&#39;s&lt;/a&gt;"
        );
        assert_eq!(escape("plain ✓"), "plain ✓");
    }

    #[test]
    fn every_page_offers_a_bug_report_and_a_feature_request_by_email() {
        let html = page("Browse designs", "<h1>Browse</h1>", false);
        assert!(
            html.contains("mailto:bwattendorf@gmail.com?subject=%5BMakerFoundry%5D%20Bug%20report%3A%20"),
            "{html}"
        );
        assert!(
            html.contains("mailto:bwattendorf@gmail.com?subject=%5BMakerFoundry%5D%20Feature%20request%3A%20"),
            "{html}"
        );
        assert!(html.contains(">Report a bug</a>") && html.contains(">Suggest a feature</a>"));
        assert!(
            html.contains(r#"<a href="https://www.facebook.com/makerfoundryapp" rel="noopener">Facebook</a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<a href="https://www.youtube.com/@MakerFoundryApp" rel="noopener">YouTube</a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<a href="https://buymeacoffee.com/bwattendorf" rel="noopener">Buy us a coffee</a>"#),
            "{html}"
        );
    }
}
