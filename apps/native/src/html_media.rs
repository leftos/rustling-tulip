//! The files an HTML page or a stylesheet names beside it, as repo-relative
//! paths to fetch with it: a page's images, clips, scripts, stylesheets and
//! fonts, read with a small scanner rather than a full HTML parser.

use std::collections::HashSet;

use crate::file_fetch::file_name;

/// The most media files one page's batch fetches.
pub const MAX_MEDIA_PER_PAGE: usize = 200;
/// The largest media file a batch fetches; a larger one is cancelled.
pub const MAX_MEDIA_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// The most bytes one page's batch fetches in all.
pub const MAX_MEDIA_BATCH_BYTES: u64 = 256 * 1024 * 1024;
/// How many media fetches a batch keeps in flight at once.
pub const MEDIA_FETCHES_IN_FLIGHT: usize = 4;

/// What a reference comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The repo-relative path to fetch, `/`-separated, with no empty, `.` or
    /// `..` segment.
    Fetch(String),
    /// Nothing to fetch: external, embedded, outside the repo or unreadable.
    Skip,
}

/// The `rel` tokens whose `<link href>` names a file the page loads.
const LINK_RELS: [&str; 5] = [
    "stylesheet",
    "icon",
    "apple-touch-icon",
    "preload",
    "modulepreload",
];

/// The character references decoded in an attribute value.
const ENTITIES: [(&str, char); 8] = [
    ("&amp;", '&'),
    ("&#38;", '&'),
    ("&#x26;", '&'),
    ("&quot;", '"'),
    ("&#39;", '\''),
    ("&apos;", '\''),
    ("&lt;", '<'),
    ("&gt;", '>'),
];

/// Is this repo-relative path an HTML page whose media should be fetched
/// (`.html` / `.htm`, any case)?
#[must_use]
pub fn is_html_page(rel: &str) -> bool {
    has_extension(rel, &["html", "htm"])
}

/// Is this repo-relative path a stylesheet to follow one level (`.css`, any
/// case)?
#[must_use]
pub fn is_stylesheet(rel: &str) -> bool {
    has_extension(rel, &["css"])
}

fn has_extension(rel: &str, extensions: &[&str]) -> bool {
    file_name(rel)
        .rsplit_once('.')
        .is_some_and(|(_, extension)| {
            extensions
                .iter()
                .any(|wanted| extension.eq_ignore_ascii_case(wanted))
        })
}

/// The raw references an HTML page names, in document order, deduplicated.
///
/// Comments and `<script>` bodies are not read; a `<style>` body is read for
/// its CSS references only.
#[must_use]
pub fn html_references(html: &str) -> Vec<String> {
    let mut found = Found::default();
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        if let Some(comment) = after.strip_prefix("!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let Some(tag) = start_tag(after) else {
            rest = after;
            continue;
        };
        tag_references(&tag, &mut found);
        rest = match tag.name.as_str() {
            "script" => raw_text(tag.rest, "script").1,
            "style" => {
                let (body, after_style) = raw_text(tag.rest, "style");
                found.extend(css_references(body));
                after_style
            }
            _ => tag.rest,
        };
    }
    found.list
}

/// The raw `url()` / `@import` references in CSS text, in order,
/// deduplicated; comments are not read.
#[must_use]
pub fn css_references(css: &str) -> Vec<String> {
    let lower = css.to_ascii_lowercase();
    let mut found = Found::default();
    let mut at = 0;
    while let Some((start, token)) = next_css_token(&lower, at) {
        at = match token {
            CssToken::Comment => lower[start + 2..]
                .find("*/")
                .map_or(css.len(), |end| start + 2 + end + 2),
            CssToken::Url => {
                let (value, resume) = url_argument(css, start + "url(".len());
                found.push(value);
                resume
            }
            CssToken::Import => {
                let from = start + "@import".len();
                let body = &css[from..];
                let lead = body.len() - body.trim_start().len();
                match split_quoted(&body[lead..]) {
                    Some((value, after)) => {
                        found.push(value);
                        css.len() - after.len()
                    }
                    None => from,
                }
            }
        };
    }
    found.list
}

/// Resolve a raw reference against the repo-relative path of the file that
/// named it: its query string and fragment dropped, `%XX` decoded, `\` read
/// as `/`, and `.` and `..` resolved against that file's folder.
///
/// External (`scheme:`, `//host`), embedded (`data:`), site-absolute,
/// drive-led and `\`-led references, ones that climb above the repo root,
/// ones naming a folder, and ones with a bad `%XX` sequence or non-UTF-8
/// name are [`Resolved::Skip`].
#[must_use]
pub fn resolve(from_rel: &str, reference: &str) -> Resolved {
    resolve_path(from_rel, reference).map_or(Resolved::Skip, Resolved::Fetch)
}

/// Every fetchable repo-relative path an HTML page names
/// ([`html_references`] resolved, deduplicated), and how many references
/// were skipped. The page itself is never listed, and a reference that
/// needs nothing (external, embedded, or within the page) is not counted.
#[must_use]
pub fn page_media(page_rel: &str, html: &str) -> (Vec<String>, usize) {
    resolve_all(page_rel, &html_references(html))
}

/// The same as [`page_media`] for a stylesheet's text, resolved against the
/// stylesheet's own folder.
#[must_use]
pub fn stylesheet_media(css_rel: &str, css: &str) -> (Vec<String>, usize) {
    resolve_all(css_rel, &css_references(css))
}

fn resolve_all(from_rel: &str, references: &[String]) -> (Vec<String>, usize) {
    let own = from_rel.replace('\\', "/");
    let mut found = Found::default();
    let mut skipped = 0;
    for reference in references {
        match resolve(from_rel, reference) {
            Resolved::Fetch(path) if path != own => found.push(&path),
            Resolved::Fetch(_) => {}
            Resolved::Skip if needs_nothing(reference) => {}
            Resolved::Skip => skipped += 1,
        }
    }
    (found.list, skipped)
}

fn resolve_path(from_rel: &str, reference: &str) -> Option<String> {
    let reference = reference.trim_matches(is_space);
    if needs_nothing(reference) {
        return None;
    }
    let path = reference
        .find(['?', '#'])
        .map_or(reference, |end| &reference[..end]);
    let path = percent_decode(path)?.replace('\\', "/");
    let names_a_folder = path
        .rsplit('/')
        .next()
        .is_some_and(|last| matches!(last, "" | "." | ".."));
    if path.starts_with('/') || has_drive(&path) || names_a_folder {
        return None;
    }
    let mut segments: Vec<&str> = from_rel
        .split(['/', '\\'])
        .filter(|segment| !matches!(*segment, "" | "."))
        .collect();
    segments.pop();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            name => segments.push(name),
        }
    }
    Some(segments.join("/"))
}

/// Whether `reference` names nothing to fetch beside the page: empty, a
/// fragment or query of the page itself, protocol-relative, or with a
/// scheme (`data:`, `http:`, `mailto:`, ...).
fn needs_nothing(reference: &str) -> bool {
    let reference = reference.trim_matches(is_space);
    reference.is_empty()
        || reference.starts_with(['#', '?'])
        || reference.starts_with("//")
        || has_scheme(reference)
}

/// Whether `reference` starts with a URL scheme; a single letter before the
/// `:` is a Windows drive, not a scheme.
fn has_scheme(reference: &str) -> bool {
    reference.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

fn has_drive(path: &str) -> bool {
    let mut chars = path.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':')
}

/// `text` with each `%XX` decoded; `None` for a broken sequence or a result
/// that is not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = input.next().and_then(hex_digit)?;
            let low = input.next().and_then(hex_digit)?;
            bytes.push(high << 4 | low);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

fn is_space(c: char) -> bool {
    c.is_ascii_whitespace()
}

/// References in the order first seen, each once.
#[derive(Default)]
struct Found {
    list: Vec<String>,
    seen: HashSet<String>,
}

impl Found {
    fn push(&mut self, reference: &str) {
        let reference = reference.trim_matches(is_space);
        if !reference.is_empty() && self.seen.insert(reference.to_owned()) {
            self.list.push(reference.to_owned());
        }
    }

    fn extend(&mut self, references: Vec<String>) {
        for reference in references {
            self.push(&reference);
        }
    }
}

/// A start tag: its lower-cased name, its attributes (names lower-cased,
/// values decoded) and the text after it.
struct Tag<'a> {
    name: String,
    attributes: Vec<(String, String)>,
    rest: &'a str,
}

impl Tag<'_> {
    fn value(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(attribute, _)| attribute == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the attribute `attribute` of this tag names one file to fetch.
    fn names_reference(&self, attribute: &str) -> bool {
        match (self.name.as_str(), attribute) {
            ("img" | "source" | "video" | "audio" | "track" | "embed" | "script", "src")
            | ("video", "poster")
            | ("object", "data") => true,
            ("input", "src") => self
                .value("type")
                .is_some_and(|kind| kind.trim_matches(is_space).eq_ignore_ascii_case("image")),
            ("link", "href") => self.value("rel").is_some_and(|rel| {
                rel.split_ascii_whitespace().any(|token| {
                    LINK_RELS
                        .iter()
                        .any(|listed| token.eq_ignore_ascii_case(listed))
                })
            }),
            _ => false,
        }
    }
}

/// The start tag at the front of `text` (the text just past a `<`), or
/// `None` when no tag name starts there.
fn start_tag(text: &str) -> Option<Tag<'_>> {
    if !text.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let name_end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(text.len());
    let name = text[..name_end].to_ascii_lowercase();
    let mut rest = &text[name_end..];
    let mut attributes = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| is_space(c) || c == '/');
        if let Some(after) = rest.strip_prefix('>') {
            rest = after;
            break;
        }
        if rest.is_empty() {
            break;
        }
        let (attribute, after) = attribute(rest);
        attributes.push(attribute);
        rest = after;
    }
    Some(Tag {
        name,
        attributes,
        rest,
    })
}

/// The attribute at the front of `text` as `(name, value)`, and the text
/// after it; a valueless attribute has an empty value.
fn attribute(text: &str) -> ((String, String), &str) {
    let name_end = text
        .find(|c: char| is_space(c) || matches!(c, '=' | '>' | '/'))
        .unwrap_or(text.len())
        .max(text.chars().next().map_or(0, char::len_utf8));
    let name = text[..name_end].to_ascii_lowercase();
    let rest = text[name_end..].trim_start_matches(is_space);
    let Some(after_equals) = rest.strip_prefix('=') else {
        return ((name, String::new()), rest);
    };
    let value_text = after_equals.trim_start_matches(is_space);
    let (raw, rest) = split_quoted(value_text).unwrap_or_else(|| {
        let end = value_text
            .find(|c: char| is_space(c) || c == '>')
            .unwrap_or(value_text.len());
        value_text.split_at(end)
    });
    ((name, decode_entities(raw)), rest)
}

/// The text inside the quotes at the front of `text` (`"` or `'`), and the
/// text after the closing quote; an unclosed quote runs to the end.
fn split_quoted(text: &str) -> Option<(&str, &str)> {
    let quote = text.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let body = &text[1..];
    Some(
        body.find(quote)
            .map_or((body, ""), |end| (&body[..end], &body[end + 1..])),
    )
}

fn decode_entities(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(amp) = rest.find('&') {
        decoded.push_str(&rest[..amp]);
        rest = &rest[amp..];
        if let Some((entity, c)) = ENTITIES.iter().find(|(entity, _)| rest.starts_with(entity)) {
            decoded.push(*c);
            rest = &rest[entity.len()..];
        } else {
            decoded.push('&');
            rest = &rest[1..];
        }
    }
    decoded.push_str(rest);
    decoded
}

/// The references a start tag's attributes name, in attribute order.
fn tag_references(tag: &Tag<'_>, found: &mut Found) {
    for (name, value) in &tag.attributes {
        if name == "style" {
            found.extend(css_references(value));
        } else if name == "srcset" && matches!(tag.name.as_str(), "img" | "source") {
            for candidate in value.split(',') {
                found.push(candidate.split_ascii_whitespace().next().unwrap_or(""));
            }
        } else if tag.names_reference(name) {
            found.push(value);
        }
    }
}

/// The body of a raw-text element (`<script>`, `<style>`) up to its end
/// tag, and the text from that end tag on.
fn raw_text<'a>(text: &'a str, name: &str) -> (&'a str, &'a str) {
    let end = text
        .to_ascii_lowercase()
        .find(&format!("</{name}"))
        .unwrap_or(text.len());
    text.split_at(end)
}

enum CssToken {
    Comment,
    Url,
    Import,
}

/// The first comment, `url(` or `@import` in the lower-cased CSS `lower`
/// from `at`, with where it starts.
fn next_css_token(lower: &str, at: usize) -> Option<(usize, CssToken)> {
    let rest = lower.get(at..)?;
    [
        ("/*", CssToken::Comment),
        ("url(", CssToken::Url),
        ("@import", CssToken::Import),
    ]
    .into_iter()
    .filter_map(|(needle, token)| rest.find(needle).map(|start| (at + start, token)))
    .min_by_key(|(start, _)| *start)
}

/// The argument of the `url(` whose `(` ends at `from`, and where to read
/// on: past its `)`.
fn url_argument(css: &str, from: usize) -> (&str, usize) {
    let body = &css[from..];
    let lead = body.len() - body.trim_start().len();
    if let Some((value, after)) = split_quoted(&body[lead..]) {
        let resume = after
            .find(')')
            .map_or(css.len(), |close| css.len() - after.len() + close + 1);
        return (value, resume);
    }
    match body.find(')') {
        Some(close) => (&body[..close], from + close + 1),
        None => (body, css.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_fetch::rel_segments;

    const PAGE: &str = "docs/a/page.html";

    fn fetch(path: &str) -> Resolved {
        Resolved::Fetch(path.to_owned())
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&item| item.to_owned()).collect()
    }

    #[test]
    fn an_img_src_is_a_reference() {
        let html = r#"<p>A cat: <img src="cat.png" alt="cat"></p>"#;
        assert_eq!(html_references(html), strings(&["cat.png"]));
        assert_eq!(page_media(PAGE, html), (strings(&["docs/a/cat.png"]), 0));
    }

    #[test]
    fn a_dot_slash_reference_resolves_beside_the_page() {
        assert_eq!(resolve(PAGE, "./support.js"), fetch("docs/a/support.js"));
        assert_eq!(resolve(PAGE, "./img/./x.png"), fetch("docs/a/img/x.png"));
    }

    #[test]
    fn a_parent_reference_resolves_against_the_repo_root() {
        let page = "docs/design/a/index.html";
        assert_eq!(
            resolve(page, "../b/img.png"),
            fetch("docs/design/b/img.png")
        );
        assert_eq!(
            resolve(page, "../../../apps/native/assets/fonts/x.ttf"),
            fetch("apps/native/assets/fonts/x.ttf")
        );
        assert_eq!(
            resolve(page, "../../../../apps/native/assets/fonts/x.ttf"),
            Resolved::Skip
        );
    }

    #[test]
    fn a_query_string_and_fragment_are_dropped() {
        assert_eq!(
            resolve(PAGE, "img/x.png?v=3#top"),
            fetch("docs/a/img/x.png")
        );
        assert_eq!(resolve(PAGE, "icons.svg#close"), fetch("docs/a/icons.svg"));
        assert_eq!(
            resolve(PAGE, "font.woff2#iefix?x"),
            fetch("docs/a/font.woff2")
        );
    }

    #[test]
    fn a_percent_encoded_name_is_decoded() {
        assert_eq!(
            resolve(PAGE, "my%20pic%C3%A9.png"),
            fetch("docs/a/my pic\u{e9}.png")
        );
        assert_eq!(resolve(PAGE, "bad%zz.png"), Resolved::Skip);
        assert_eq!(resolve(PAGE, "cut%2.png"), Resolved::Skip);
        assert_eq!(resolve(PAGE, "latin%FF.png"), Resolved::Skip);
    }

    #[test]
    fn srcset_candidates_lose_their_descriptors() {
        let html = r#"<img srcset="a.png 1x, b@2x.png 2x,c.png 480w , ">
            <picture><source srcset=" wide.webp   1200w"></picture>"#;
        assert_eq!(
            html_references(html),
            strings(&["a.png", "b@2x.png", "c.png", "wide.webp"])
        );
    }

    #[test]
    fn video_src_and_poster_are_references() {
        let html = r#"<video src="clip.mp4" poster="still.jpg">
            <source src="clip.webm"><track src="subs.vtt"></video>
            <audio src="sound.ogg"></audio>"#;
        assert_eq!(
            html_references(html),
            strings(&[
                "clip.mp4",
                "still.jpg",
                "clip.webm",
                "subs.vtt",
                "sound.ogg"
            ])
        );
    }

    #[test]
    fn a_stylesheet_link_is_a_reference_and_an_anchor_is_not() {
        let html = r#"<link rel="stylesheet" href="site.css">
            <link rel="canonical" href="canon.html">
            <a href="other.html">other</a><iframe src="frame.html"></iframe>"#;
        assert_eq!(html_references(html), strings(&["site.css"]));
    }

    #[test]
    fn a_rel_list_matches_any_listed_token() {
        let html = r#"<link rel="shortcut icon" href="fav.ico">
            <link rel="alternate stylesheet" href="alt.css">
            <link rel="apple-touch-icon" href="touch.png">
            <link rel="modulepreload" href="mod.js">
            <link rel="preload" href="font.woff2" as="font">
            <link rel="author license" href="humans.txt">"#;
        assert_eq!(
            html_references(html),
            strings(&["fav.ico", "alt.css", "touch.png", "mod.js", "font.woff2"])
        );
    }

    #[test]
    fn object_data_is_a_reference() {
        let html = r#"<object data="movie.svg" type="image/svg+xml"></object><embed src="e.pdf">
            <input type="image" src="go.png"><input type="text" src="not.png">"#;
        assert_eq!(
            html_references(html),
            strings(&["movie.svg", "e.pdf", "go.png"])
        );
    }

    #[test]
    fn url_in_a_style_element_is_a_reference() {
        let html = "<style>body { background: url(bg.png) }</style><p>url(text.png)</p>";
        assert_eq!(html_references(html), strings(&["bg.png"]));
    }

    #[test]
    fn url_in_a_style_attribute_is_a_reference() {
        let html = r#"<div style="background-image: url('tile.png')"></div>
            <span style="background: url(&quot;quoted.png&quot;)"></span>"#;
        assert_eq!(html_references(html), strings(&["tile.png", "quoted.png"]));
    }

    #[test]
    fn css_import_forms_are_references() {
        let css = "@import \"a.css\";\n@import url(b.css) screen;\n@IMPORT 'd.css';\n\
            /* url(commented.png) */\n.x { background: url( 'c.png' ) }\n\
            .y { src: url(\"e(1).woff\") format(\"woff\") }";
        assert_eq!(
            css_references(css),
            strings(&["a.css", "b.css", "d.css", "c.png", "e(1).woff"])
        );
    }

    #[test]
    fn data_and_http_and_protocol_relative_urls_need_nothing() {
        let references = [
            "data:image/png;base64,AAAA",
            "http://example.com/x.png",
            "HTTPS://example.com/x.png",
            "//cdn.example.com/x.png",
            "mailto:me@example.com",
            "javascript:void(0)",
            "blob:abc",
            "file:///C:/x.png",
            "#section",
            "?v=2",
        ];
        for reference in references {
            assert_eq!(resolve(PAGE, reference), Resolved::Skip, "{reference}");
        }
        let html: String = references
            .iter()
            .map(|reference| ["<img src=\"", reference, "\">"].concat())
            .collect();
        assert_eq!(page_media(PAGE, &html), (Vec::new(), 0));
    }

    #[test]
    fn a_reference_inside_a_script_or_comment_is_not_a_reference() {
        let html = r#"<script>
              const tag = '<img src="in-script.png">';
              el.style.background = `url(${c.toDataURL('image/png')})`;
            </script>
            <!-- <img src="commented.png"> -->
            <img src="shown.png"><script src="app.js"></script>
            <!-- unclosed <img src="tail.png">"#;
        assert_eq!(html_references(html), strings(&["shown.png", "app.js"]));
    }

    #[test]
    fn a_reference_above_the_repo_root_is_dropped() {
        assert_eq!(resolve("index.html", "../x.png"), Resolved::Skip);
        assert_eq!(resolve(PAGE, "../../../x.png"), Resolved::Skip);
        assert_eq!(resolve(PAGE, "img/../../../../x.png"), Resolved::Skip);
        assert_eq!(resolve(PAGE, "../../x.png"), fetch("x.png"));
    }

    #[test]
    fn a_site_absolute_reference_is_dropped() {
        assert_eq!(resolve(PAGE, "/img/x.png"), Resolved::Skip);
        assert_eq!(resolve(PAGE, "%2Fimg/x.png"), Resolved::Skip);
        assert_eq!(
            page_media(PAGE, r#"<img src="/img/x.png">"#),
            (Vec::new(), 1)
        );
    }

    #[test]
    fn a_drive_or_backslash_led_reference_is_dropped() {
        for reference in [
            "C:/x.png",
            "c:\\x.png",
            "\\x.png",
            "\\\\server\\share\\x.png",
        ] {
            assert_eq!(resolve(PAGE, reference), Resolved::Skip, "{reference}");
        }
        assert_eq!(resolve(PAGE, "img\\x.png"), fetch("docs/a/img/x.png"));
    }

    #[test]
    fn duplicate_references_are_listed_once() {
        let html = r#"<img src="a.png"><img src="b.png"><img src="a.png"><img src="./a.png">"#;
        assert_eq!(
            html_references(html),
            strings(&["a.png", "b.png", "./a.png"])
        );
        assert_eq!(
            page_media(PAGE, html),
            (strings(&["docs/a/a.png", "docs/a/b.png"]), 0)
        );
        let css = "a { background: url(x.png) } b { background: url(x.png) }";
        assert_eq!(css_references(css), strings(&["x.png"]));
    }

    #[test]
    fn unquoted_and_single_quoted_attributes_read() {
        let html = r#"<img src=plain.png alt=x><img src='single.png'>
            <img alt="a > b" src="after.png"><img hidden src = "spaced.png">"#;
        assert_eq!(
            html_references(html),
            strings(&["plain.png", "single.png", "after.png", "spaced.png"])
        );
    }

    #[test]
    fn entities_in_attribute_values_are_decoded() {
        let html = r#"<img src="a&amp;b.png"><img src="c&#38;d.png"><img src="e&#x26;f.png">
            <img src='q&quot;.png'><img src="s&#39;&apos;.png"><img src="l&lt;g&gt;.png">
            <img src="keep&copy;.png">"#;
        assert_eq!(
            html_references(html),
            strings(&[
                "a&b.png",
                "c&d.png",
                "e&f.png",
                "q\".png",
                "s''.png",
                "l<g>.png",
                "keep&copy;.png"
            ])
        );
    }

    #[test]
    fn uppercase_tags_and_attributes_read() {
        let html = r#"<IMG SRC="UP.PNG"><Link REL="StyleSheet" HREF="s.css">
            <STYLE>B { background: URL(bg.png) }</STYLE>
            <SCRIPT>var x = '<img src="no.png">';</SCRIPT><Input TYPE="IMAGE" Src="in.png">"#;
        assert_eq!(
            html_references(html),
            strings(&["UP.PNG", "s.css", "bg.png", "in.png"])
        );
    }

    #[test]
    fn css_url_resolves_against_the_css_file_not_the_page() {
        let css = "body { background: url(../img/bg.png) } @import 'more.css';";
        assert_eq!(
            stylesheet_media("docs/css/site.css", css),
            (strings(&["docs/img/bg.png", "docs/css/more.css"]), 0)
        );
        let page = format!("<style>{css}</style>");
        assert_eq!(
            page_media("docs/pages/deep/page.html", &page),
            (
                strings(&["docs/pages/img/bg.png", "docs/pages/deep/more.css"]),
                0
            )
        );
    }

    #[test]
    fn page_media_counts_what_it_skips() {
        let html = r##"<img src="/abs.png"><img src="../../../up.png"><img src="C:/x.png">
            <img src="bad%zz.png"><img src="img/"><img src="https://x.test/y.png">
            <img src="data:image/gif;base64,R0lG"><img src="#frag"><img src="ok.png">
            <img src="page.html"><img src="./page.html#top">"##;
        assert_eq!(page_media(PAGE, html), (strings(&["docs/a/ok.png"]), 5));
    }

    /// A trimmed copy of `docs/plans/canvases/tab-drag-mark/index.html`.
    const TAB_DRAG_MARK: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Tab Drag Mark</title>
<style>
  @font-face {
    font-family: "Schibsted Grotesk";
    font-weight: 400;
    src: url("../../../../apps/native/assets/fonts/SchibstedGrotesk/SchibstedGrotesk-Regular.ttf") format("truetype");
  }
  @font-face {
    font-family: "Geist Mono";
    font-weight: 400;
    src: url("../../../../apps/native/assets/fonts/GeistMono/GeistMono-Regular.ttf") format("truetype");
  }
  /* Tokens from apps/native/src/palette.rs */
  :root { --ground: #111013; }
</style>
</head>
<body>
<main>
  <h1>Where will the dragged tab land?</h1>
  <section class="lane real">
    <h2>Today (no drag)</h2>
    <img src="real-strip.png" alt="The real tab strip of the native client with two tabs, tulip and yaat">
  </section>
  <div id="lanes"></div>
</main>
<script>
  const X_ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor"><path d="M6 6l12 12"/></svg>';
</script>
</body>
</html>"#;

    #[test]
    fn the_tab_drag_mark_canvas_names_its_strip_and_fonts() {
        let page = "docs/plans/canvases/tab-drag-mark/index.html";
        assert_eq!(
            page_media(page, TAB_DRAG_MARK),
            (
                strings(&[
                    "apps/native/assets/fonts/SchibstedGrotesk/SchibstedGrotesk-Regular.ttf",
                    "apps/native/assets/fonts/GeistMono/GeistMono-Regular.ttf",
                    "docs/plans/canvases/tab-drag-mark/real-strip.png",
                ]),
                0
            )
        );
    }

    #[test]
    fn every_fetch_path_passes_rel_segments() {
        let pairs = [
            (PAGE, "./a/./b//c.png"),
            (PAGE, "a%2F..%2F..%2Fb.png"),
            (PAGE, "img\\x.png"),
            (PAGE, "../../x.png"),
            (PAGE, "my%20pic.png?x#y"),
            (PAGE, "a%2E%2E/b.png"),
            ("index.html", "x.png"),
            ("docs/design/a/index.html", "../b/img.png"),
            ("docs/css/site.css", "../img/bg.png"),
        ];
        let mut fetched = 0;
        for (from, reference) in pairs {
            if let Resolved::Fetch(path) = resolve(from, reference) {
                fetched += 1;
                assert!(
                    rel_segments(&path).is_ok(),
                    "{from} + {reference} -> {path}"
                );
            }
        }
        let (corpus, _) = page_media(
            "docs/plans/canvases/tab-drag-mark/index.html",
            TAB_DRAG_MARK,
        );
        for path in &corpus {
            assert!(rel_segments(path).is_ok(), "{path}");
        }
        assert_eq!(fetched, pairs.len());
        assert_eq!(corpus.len(), 3);
    }
}
