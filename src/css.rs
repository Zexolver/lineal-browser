//! Just enough CSS to know what a page hides.
//!
//! Lineal doesn't do visual styling, but pages hide lots of things (menus, modals, duplicate
//! mobile/desktop markup) with stylesheets. We pull out the selectors of rules that hide
//! elements and match them against the DOM.

use std::collections::HashSet;

use url::Url;

/// Media queries are evaluated as if the page were shown in a window this wide (CSS px):
/// desktop markup tends to keep navigation reachable without JavaScript.
const VIEWPORT_PX: f32 = 1024.0;

/// Selectors of rules in `css` that hide elements (`display:none`, `visibility:hidden`, `opacity:0`).
pub fn hiding_selectors(css: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect(&strip_comments(css), &mut out);
    out
}

fn collect(css: &str, out: &mut Vec<String>) {
    let mut rest = css;
    while let Some(open) = rest.find('{') {
        // Statements like `@import url(x);` or `@charset "x";` end up in front of the next prelude.
        let prelude = rest[..open].rsplit(';').next().unwrap_or("").trim();
        let close = matching_brace(rest, open);
        let body = &rest[(open + 1).min(close)..close];
        if let Some(at) = prelude.strip_prefix('@') {
            let (name, query) = at.split_once(|c: char| c.is_whitespace() || c == '(').map_or((at, ""), |(n, _)| {
                (n, &at[n.len()..])
            });
            match name.to_ascii_lowercase().as_str() {
                "media" if media_matches(query) => collect(body, out),
                "supports" | "layer" | "scope" | "document" => collect(body, out),
                _ => {}
            }
        } else if !prelude.is_empty() && hides(body) {
            out.push(prelude.to_string());
        }
        rest = &rest[(close + 1).min(rest.len())..];
    }
}

fn matching_brace(s: &str, open: usize) -> usize {
    let mut depth = 0;
    for (i, c) in s[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + i;
                }
            }
            _ => {}
        }
    }
    s.len()
}

fn hides(decls: &str) -> bool {
    if decls.contains('{') {
        return false; // nested rules; not worth handling
    }
    let decls: String = decls.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    // `opacity: 0` also hides, unless the rule animates it into view (fade-ins).
    let animated = decls.contains("animation") || decls.contains("transition");
    decls.split(';').any(|d| {
        let d = d.trim_end_matches("!important");
        d == "display:none" || d == "visibility:hidden" || (!animated && (d == "opacity:0" || d == "opacity:0.0"))
    })
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = rest[start + 2..].find("*/").map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    out.push_str(rest);
    out
}

/// Evaluates a media query list for a screen `VIEWPORT_PX` wide. Unknown features count as true.
pub fn media_matches(query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    q.split(',').any(|part| {
        let part = part.trim();
        let (negate, part) = match part.strip_prefix("not ") {
            Some(p) => (true, p.trim()),
            None => (false, part),
        };
        let part = part.strip_prefix("only ").unwrap_or(part).trim();
        let media_type = part.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("");
        let type_ok = matches!(media_type, "" | "all" | "screen" | "and");
        let features_ok = part.split('(').skip(1).all(|f| feature_matches(f.split(')').next().unwrap_or("")));
        (type_ok && features_ok) != negate
    })
}

fn feature_matches(f: &str) -> bool {
    let f: String = f.chars().filter(|c| !c.is_whitespace()).collect();
    let px = |v: &str| -> Option<f32> {
        let num_end = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
        let n: f32 = v[..num_end].parse().ok()?;
        Some(match &v[num_end..] {
            "em" | "rem" => n * 16.0,
            _ => n,
        })
    };
    if let Some(v) = f.strip_prefix("min-width:") {
        return px(v).is_none_or(|v| VIEWPORT_PX >= v);
    }
    if let Some(v) = f.strip_prefix("max-width:") {
        return px(v).is_none_or(|v| VIEWPORT_PX <= v);
    }
    // Range syntax: `width >= 600px`, `width < 40em`.
    if let Some(rest) = f.strip_prefix("width") {
        for (op, cmp) in [(">=", 0), ("<=", 1), (">", 2), ("<", 3)] {
            if let Some(v) = rest.strip_prefix(op) {
                return px(v).is_none_or(|v| match cmp {
                    0 => VIEWPORT_PX >= v,
                    1 => VIEWPORT_PX <= v,
                    2 => VIEWPORT_PX > v,
                    _ => VIEWPORT_PX < v,
                });
            }
        }
    }
    !f.starts_with("prefers-reduced-data")
}

/// Cheap pre-check before handing a selector to the real matcher: every class and id in the
/// last compound of some comma-separated part must exist in the document.
pub fn could_match(selector: &str, classes: &HashSet<String>, ids: &HashSet<String>) -> bool {
    selector.split(',').any(|part| {
        // Drop `:not(...)`, `[attr=...]` and friends so their contents don't count.
        let mut clean = String::new();
        let mut depth = 0;
        for c in part.chars() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                _ if depth == 0 => clean.push(c),
                _ => {}
            }
        }
        let last = clean.trim().rsplit([' ', '>', '+', '~', '\t', '\n']).next().unwrap_or("");
        let mut ok = true;
        let mut chars = last.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c != '.' && c != '#' {
                continue;
            }
            let name: String = last[i + 1..].chars().take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
            if name.is_empty() {
                continue;
            }
            let found = if c == '.' { classes.contains(&name) } else { ids.contains(&name) };
            ok &= found;
            for _ in 0..name.chars().count() {
                chars.next();
            }
        }
        ok
    })
}

/// Finds `<link rel=stylesheet>` URLs with a cheap scan of the raw HTML, before parsing it.
pub fn stylesheet_links(html: &str, base: &Url) -> Vec<Url> {
    let lower = html.to_ascii_lowercase();
    let end_of_head = lower.find("<body").unwrap_or(lower.len());
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<link") {
        let start = pos + i;
        if start > end_of_head && out.len() > 8 {
            break;
        }
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &html[start..end];
        pos = end;
        let rel = attr(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        let rels: Vec<&str> = rel.split_whitespace().collect();
        if !rels.contains(&"stylesheet") || rels.contains(&"alternate") || attr(tag, "disabled").is_some() {
            continue;
        }
        if let Some(media) = attr(tag, "media")
            && !media_matches(&media)
        {
            continue;
        }
        if let Some(url) = attr(tag, "href").and_then(|h| base.join(h.trim()).ok())
            && matches!(url.scheme(), "http" | "https")
        {
            out.push(url);
        }
    }
    out
}

/// Reads an attribute from a raw start tag (quoted or unquoted). Entities are decoded only for `&amp;`.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at > 0 && (bytes[at - 1].is_ascii_whitespace() || bytes[at - 1] == b'"' || bytes[at - 1] == b'\'');
        let rest = lower[from..].trim_start();
        if !before_ok {
            continue;
        }
        let Some(after_eq) = rest.strip_prefix('=') else {
            let next = rest.chars().next();
            if next.is_none_or(|c| c.is_whitespace() || c == '/' || c == '>') {
                return Some(String::new());
            }
            continue;
        };
        let value_start = tag.len() - after_eq.trim_start().len();
        let raw = &tag[value_start..];
        let value = match raw.chars().next() {
            Some(q @ ('"' | '\'')) => raw[1..].split(q).next().unwrap_or(""),
            _ => raw.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
        };
        return Some(value.replace("&amp;", "&"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_hiding_rules() {
        let css = "@charset \"utf-8\"; /* x { display:none } */ .a, .b { color: red; display : none } \
            @media (max-width: 600px) { .mobile { display:none } } @media screen and (min-width: 40em) { .wide { display: none!important } } \
            @media print { .p { display:none } } .d{display:block} .e{visibility:hidden} @keyframes k { from { opacity: 0 } } \
            .f { opacity: 0; position: absolute } .g { opacity: 0; transition: opacity .2s }";
        assert_eq!(hiding_selectors(css), vec![".a, .b", ".wide", ".e", ".f"]);
    }

    #[test]
    fn media_queries() {
        assert!(media_matches("screen"));
        assert!(media_matches("all and (min-width: 768px)"));
        assert!(!media_matches("print"));
        assert!(!media_matches("(max-width: 600px)"));
        assert!(media_matches("print, screen"));
        assert!(media_matches("not print"));
        assert!(media_matches("(width >= 600px)"));
        assert!(!media_matches("(width < 60em)"));
    }

    #[test]
    fn selector_prefilter() {
        let classes: HashSet<String> = ["menu".to_string()].into();
        let ids: HashSet<String> = ["top".to_string()].into();
        assert!(could_match(".menu", &classes, &ids));
        assert!(could_match("div .menu:not(.open)", &classes, &ids));
        assert!(!could_match(".menu.nope", &classes, &ids));
        assert!(could_match(".nope .menu, #x", &classes, &ids));
        assert!(!could_match("#x", &classes, &ids));
        assert!(could_match("#top > li", &classes, &ids));
    }

    #[test]
    fn stylesheet_link_scan() {
        let base = Url::parse("https://example.org/a/").unwrap();
        let html = r#"<head><link rel="stylesheet" href="s.css?a=1&amp;b=2"><link rel=icon href=i.png>
            <LINK REL='alternate stylesheet' href=alt.css><link href=/p.css rel=stylesheet media=print>
            <link rel=stylesheet href=//cdn.example.org/x.css media="screen and (min-width: 100px)">"#;
        let links: Vec<String> = stylesheet_links(html, &base).iter().map(|u| u.to_string()).collect();
        assert_eq!(links, vec!["https://example.org/a/s.css?a=1&b=2", "https://cdn.example.org/x.css"]);
    }
}
