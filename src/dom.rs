//! A parsed page: the html5ever DOM plus everything the layout engine needs about it.

use std::collections::HashSet;

use ego_tree::NodeId;
use html5ever::driver::{self, ParseOpts};
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use scraper::{Html, HtmlTreeSink, Node, Selector};
use url::Url;

use crate::forms::Forms;
use crate::layout::{self, Ctx, Layout};

/// Class names that sites use for "visually hidden, screen readers only" text.
const SR_ONLY: &[&str] = &["sr-only", "visually-hidden", "visuallyhidden", "screen-reader-text", "screenreader-only", "a11y-hidden", "hidden"];

/// Stop matching CSS rules after this many selectors, so a huge inline sheet can't stall us.
const MAX_SELECTORS: usize = 3000;

pub struct Dom {
    pub html: Html,
    pub url: Url,
    pub base: Url,
    pub title: String,
    pub hidden: HashSet<NodeId>,
    pub forms: Forms,
    /// `<meta http-equiv=refresh>`: (delay in seconds, target).
    pub refresh: Option<(u64, Url)>,
}

impl Dom {
    pub fn parse(source: &str, url: &Url) -> Dom {
        // Scripting is off: we don't run JS, so `<noscript>` content must be parsed as real markup.
        let opts = ParseOpts {
            tree_builder: TreeBuilderOpts { scripting_enabled: false, ..Default::default() },
            ..Default::default()
        };
        let html = driver::parse_document(HtmlTreeSink::new(Html::new_document()), opts).one(source);

        let mut title = String::new();
        let mut base = url.clone();
        let mut refresh = None;
        let mut sheets = Vec::new();
        for node in html.tree.root().descendants() {
            let Some(e) = node.value().as_element() else { continue };
            match e.name() {
                "title" if title.is_empty() && !inside(node, "svg") => {
                    title = text_of(node).split_whitespace().collect::<Vec<_>>().join(" ");
                }
                "base" => {
                    if let Some(b) = e.attr("href").and_then(|h| url.join(h.trim()).ok()) {
                        base = b;
                    }
                }
                "meta" if e.attr("http-equiv").is_some_and(|h| h.eq_ignore_ascii_case("refresh")) => {
                    refresh = e.attr("content").and_then(|c| parse_refresh(c, url));
                }
                "style" => {
                    let media = e.attr("media").unwrap_or("all").to_ascii_lowercase();
                    if media.contains("all") || media.contains("screen") {
                        sheets.push(text_of(node));
                    }
                }
                _ => {}
            }
        }

        let mut hidden = HashSet::new();
        let mut selectors: Vec<String> = SR_ONLY.iter().map(|c| format!(".{c}")).collect();
        for sheet in &sheets {
            selectors.extend(display_none_selectors(sheet));
        }
        for sel in selectors.iter().take(MAX_SELECTORS) {
            if let Ok(sel) = Selector::parse(sel) {
                hidden.extend(html.select(&sel).map(|e| e.id()));
            }
        }

        let forms = Forms::build(&html);
        Dom { html, url: url.clone(), base, title, hidden, forms, refresh }
    }

    pub fn layout(&self, width: usize, number_links: bool) -> Layout {
        let ctx = Ctx { forms: &self.forms, hidden: &self.hidden, base: &self.base, number_links };
        layout::layout(&self.html, &ctx, width)
    }
}

fn inside(node: ego_tree::NodeRef<Node>, name: &str) -> bool {
    node.ancestors().any(|a| a.value().as_element().is_some_and(|e| e.name() == name))
}

pub fn text_of(node: ego_tree::NodeRef<Node>) -> String {
    node.descendants().filter_map(|n| n.value().as_text().map(|t| t.to_string())).collect()
}

/// `"5; url=/next"` -> (5, base.join("/next")).
fn parse_refresh(content: &str, base: &Url) -> Option<(u64, Url)> {
    let (delay, rest) = content.split_once([';', ',']).unwrap_or((content, ""));
    let delay = delay.trim().split('.').next()?.parse().ok()?;
    let rest = rest.trim();
    let target = match rest.split_once('=') {
        Some((k, v)) if k.trim().eq_ignore_ascii_case("url") => v.trim().trim_matches(['"', '\'']),
        _ => rest,
    };
    if target.is_empty() {
        return None;
    }
    Some((delay, base.join(target).ok()?))
}

/// Selectors of top-level rules in `css` that declare `display: none`.
///
/// Rules inside at-rules (`@media` etc.) are skipped: whether they apply depends on a viewport we don't have.
pub fn display_none_selectors(css: &str) -> Vec<String> {
    let css = strip_comments(css);
    let mut out = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let prelude = rest[..open].trim();
        // Find the matching close brace.
        let mut depth = 0;
        let mut close = None;
        for (i, c) in rest[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.unwrap_or(rest.len());
        let body = &rest[(open + 1).min(close)..close];
        // A prelude may start with leftovers of a `@import ...;` statement.
        let prelude = prelude.rsplit(';').next().unwrap_or("").trim();
        if !prelude.starts_with('@') {
            let decls: String = body.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
            if decls.split(';').any(|d| d == "display:none" || d == "display:none!important") {
                out.push(prelude.to_string());
            }
        }
        rest = &rest[(close + 1).min(rest.len())..];
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_display_none() {
        let css = "/* x { display:none } */ .a, .b { color: red; display : none } @media (max-width: 9px) { .c { display:none } } .d{display:block} .e{display:none!important}";
        assert_eq!(display_none_selectors(css), vec![".a, .b", ".e"]);
    }

    #[test]
    fn page_metadata() {
        let url = Url::parse("https://example.org/dir/page").unwrap();
        let dom = Dom::parse(
            r#"<title> Hello
            World </title><base href="/root/"><meta http-equiv="Refresh" content="0; URL='next.html'">
            <style>.gone{display:none}</style><p class=gone>x</p><noscript><p>no js</p></noscript>"#,
            &url,
        );
        assert_eq!(dom.title, "Hello World");
        assert_eq!(dom.base.as_str(), "https://example.org/root/");
        assert_eq!(dom.refresh.as_ref().map(|(d, u)| (*d, u.as_str())), Some((0, "https://example.org/dir/next.html")));
        assert_eq!(dom.hidden.len(), 1);
        let text = dom.layout(80, false).plain_text();
        assert!(text.contains("no js") && !text.contains('x'), "{text}");
    }
}
