//! The `$('#id').html()` subset of cheerio that the portal page needs
//! (SPEC §3.2).
//!
//! The reference decodes the server configuration with `$('#acid').html()` and
//! friends. This module reproduces exactly that operation on the raw HTML
//! text: find the first start tag whose `id` attribute is the requested value
//! and return its **raw, untrimmed inner HTML**. There is no entity decoding,
//! no whitespace normalisation and no attribute name folding beyond the
//! case-insensitive tag-name comparison — the HTML grammar lowercases tag
//! names, so `<SPAN id="x">` and `<span id="x">` are the same element.
//!
//! A missing element yields [`None`] — cheerio's `null`, which the caller must
//! distinguish from an element that exists but is empty ([`Some`] of an empty
//! string, which is also the answer for void and `/>`-closed elements).

/// HTML void elements: their start tag has no inner HTML, so
/// `$('#id').html()` is the empty string (SPEC §3.2).
const VOID_ELEMENTS: &[&str] = &[
    "input", "img", "br", "meta", "link", "hr", "source", "area", "base", "col", "embed", "param",
    "track", "wbr",
];

/// `$('#{id}').html()`: the raw inner HTML of the first element carrying the
/// `id` attribute `id`, or `None` when the page has no such element.
pub fn element_html(html: &str, id: &str) -> Option<String> {
    let mut i = 0;
    while i < html.len() {
        match scan(html, i) {
            Scan::Start(tag) => {
                if tag.id == Some(id) {
                    // First match wins. A void element or a `/>` start tag has
                    // no content at all.
                    if tag.self_closing || is_void(tag.name) {
                        return Some(String::new());
                    }
                    return Some(inner(html, tag.end, tag.name));
                }
                i = tag.end;
            }
            Scan::Close { end, .. } | Scan::Skip(end) => i = end,
            Scan::None => i += 1,
        }
    }
    None
}

/// The inner HTML of the element whose start tag ends at `start` (just past
/// its `>`), up to the end tag that closes it.
///
/// Same-name nested tags are balanced, so `<div id="x">a<div>b</div>c</div>`
/// yields `a<div>b</div>c`. An element that is never closed runs to the end of
/// the document, which is what an HTML parser does with it.
fn inner(html: &str, start: usize, name: &str) -> String {
    let mut depth = 0usize;
    let mut i = start;
    while i < html.len() {
        match scan(html, i) {
            Scan::Start(tag) => {
                if tag.name.eq_ignore_ascii_case(name) && !tag.self_closing && !is_void(tag.name) {
                    depth += 1;
                }
                i = tag.end;
            }
            Scan::Close { name: close, end } => {
                if close.eq_ignore_ascii_case(name) {
                    if depth == 0 {
                        return html[start..i].to_string();
                    }
                    depth -= 1;
                }
                i = end;
            }
            Scan::Skip(end) => i = end,
            Scan::None => i += 1,
        }
    }
    html[start..].to_string()
}

/// What lies at one `<`.
enum Scan<'a> {
    /// A start tag, `<div …>`.
    Start(StartTag<'a>),
    /// An end tag, `</div>`; `end` is one past its `>`.
    Close { name: &'a str, end: usize },
    /// A comment, doctype, CDATA section or unterminated tag: never an element.
    Skip(usize),
    /// Not a tag start.
    None,
}

struct StartTag<'a> {
    name: &'a str,
    /// The value of the `id` attribute, when the tag carries a whole
    /// attribute named exactly `id` (`data-id` is a different name).
    id: Option<&'a str>,
    /// One past the closing `>`.
    end: usize,
    /// The tag ended in `/>`.
    self_closing: bool,
}

/// Parse the tag starting at `i` (a byte index; anything that is not `<` gives
/// [`Scan::None`]).
fn scan(html: &str, i: usize) -> Scan<'_> {
    let bytes = html.as_bytes();
    if i + 1 >= bytes.len() || bytes[i] != b'<' {
        return Scan::None;
    }
    // Comments are skipped whole, so a tag inside one is never an element.
    if html[i..].starts_with("<!--") {
        return match html[i + 4..].find("-->") {
            Some(rel) => Scan::Skip(i + 4 + rel + 3),
            None => Scan::Skip(bytes.len()),
        };
    }
    // Doctype, CDATA, bogus comment.
    if bytes[i + 1] == b'!' {
        return Scan::Skip(skip_to_gt(html, i));
    }
    if bytes[i + 1] == b'/' {
        let mut k = i + 2;
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        let name_start = k;
        while k < bytes.len() && is_name_byte(bytes[k]) {
            k += 1;
        }
        if k == name_start {
            return Scan::None;
        }
        return Scan::Close {
            name: &html[name_start..k],
            end: skip_to_gt(html, k),
        };
    }
    if !bytes[i + 1].is_ascii_alphabetic() {
        return Scan::None;
    }
    let name_start = i + 1;
    let mut k = name_start;
    while k < bytes.len() && is_name_byte(bytes[k]) {
        k += 1;
    }
    let name = &html[name_start..k];

    let mut id = None;
    let mut self_closing = false;
    loop {
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if k >= bytes.len() {
            // Unterminated start tag: there is no element to return.
            return Scan::Skip(bytes.len());
        }
        match bytes[k] {
            b'>' => {
                k += 1;
                break;
            }
            b'/' => {
                if k + 1 < bytes.len() && bytes[k + 1] == b'>' {
                    self_closing = true;
                    k += 2;
                    break;
                }
                k += 1; // stray solidus
                continue;
            }
            _ => {}
        }
        // Attribute name, up to `=`, whitespace, `/` or `>`.
        let attr_start = k;
        while k < bytes.len()
            && !bytes[k].is_ascii_whitespace()
            && bytes[k] != b'='
            && bytes[k] != b'>'
            && bytes[k] != b'/'
        {
            k += 1;
        }
        let attr = &html[attr_start..k];
        // Optional value, quoted or unquoted.
        let mut p = k;
        while p < bytes.len() && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        let mut value = None;
        if p < bytes.len() && bytes[p] == b'=' {
            p += 1;
            while p < bytes.len() && bytes[p].is_ascii_whitespace() {
                p += 1;
            }
            if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
                let quote = bytes[p];
                let value_start = p + 1;
                let mut v = value_start;
                while v < bytes.len() && bytes[v] != quote {
                    v += 1;
                }
                value = Some(&html[value_start..v]);
                k = if v < bytes.len() { v + 1 } else { v };
            } else {
                let mut v = p;
                while v < bytes.len() && !bytes[v].is_ascii_whitespace() && bytes[v] != b'>' {
                    v += 1;
                }
                value = Some(&html[p..v]);
                k = v;
            }
        }
        if id.is_none() && attr.eq_ignore_ascii_case("id") {
            id = value;
        }
    }
    Scan::Start(StartTag {
        name,
        id,
        end: k,
        self_closing,
    })
}

/// Index one past the next `>`, or the end of the input.
fn skip_to_gt(html: &str, from: usize) -> usize {
    match html[from..].find('>') {
        Some(rel) => from + rel + 1,
        None => html.len(),
    }
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b':'
}

fn is_void(name: &str) -> bool {
    VOID_ELEMENTS.iter().any(|v| name.eq_ignore_ascii_case(v))
}

#[cfg(test)]
mod tests {
    use super::element_html;

    #[test]
    fn reads_raw_untrimmed_inner_html() {
        let html = "<div class=\"a\">\n  <span id=\"acid\"> \"12\" </span>\n</div>";
        assert_eq!(element_html(html, "acid").as_deref(), Some(" \"12\" "));
    }

    #[test]
    fn missing_element_is_none() {
        assert_eq!(element_html("<span id=\"acid\">\"1\"</span>", "ip"), None);
        assert_eq!(element_html("", "acid"), None);
    }

    #[test]
    fn single_quoted_and_reordered_attributes_match() {
        assert_eq!(
            element_html("<p class=\"hidden\" id='acid'>\"12\"</p>", "acid").as_deref(),
            Some("\"12\"")
        );
    }

    #[test]
    fn tag_name_is_case_insensitive() {
        assert_eq!(
            element_html("<SPAN class=\"x\" ID=\"nas\">10.0.0.1</SPAN>", "nas").as_deref(),
            Some("10.0.0.1")
        );
    }

    #[test]
    fn data_id_is_not_the_id_attribute() {
        let html = "<span data-id=\"acid\">nope</span><span id=\"acid\">\"12\"</span>";
        assert_eq!(element_html(html, "acid").as_deref(), Some("\"12\""));
        assert_eq!(element_html("<span data-id=\"acid\">nope</span>", "acid"), None);
    }

    #[test]
    fn first_match_wins() {
        let html = "<i id=\"x\">one</i><i id=\"x\">two</i>";
        assert_eq!(element_html(html, "x").as_deref(), Some("one"));
    }

    #[test]
    fn void_and_self_closing_elements_are_empty() {
        assert_eq!(element_html("<input id=\"a\" value=\"1\">tail", "a").as_deref(), Some(""));
        assert_eq!(element_html("<br id=\"b\"/>tail", "b").as_deref(), Some(""));
        assert_eq!(element_html("<br id=\"c\" />tail", "c").as_deref(), Some(""));
        assert_eq!(element_html("<meta id=\"d\" charset=\"utf-8\">", "d").as_deref(), Some(""));
    }

    #[test]
    fn same_name_nesting_is_balanced() {
        let html = "<div id=\"x\">a<div>b</div>c</div>tail";
        assert_eq!(element_html(html, "x").as_deref(), Some("a<div>b</div>c"));
    }

    #[test]
    fn unclosed_element_runs_to_end_of_document() {
        assert_eq!(element_html("<div id=\"x\">tail", "x").as_deref(), Some("tail"));
    }

    #[test]
    fn comments_are_not_elements() {
        let html = "<!-- <span id=\"acid\">\"1\"</span> -->";
        assert_eq!(element_html(html, "acid"), None);
    }

    #[test]
    fn quoted_attribute_values_may_contain_angle_brackets() {
        let html = "<span title=\"a>b\" id=\"lang\">\"zh-CN\"</span>";
        assert_eq!(element_html(html, "lang").as_deref(), Some("\"zh-CN\""));
    }
}
