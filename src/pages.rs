//! Built-in `about:` pages, rendered by the same engine as any website.

use url::Url;

use crate::net::{Document, escape};

pub const KEYS: &[(&str, &str)] = &[
    ("Ctrl-L, Alt-D, F6", "Edit the address (type a URL or a search)"),
    ("Mouse", "Click links, buttons and fields; wheel to scroll"),
    ("Tab / Shift-Tab", "Move between links and form fields"),
    ("Enter", "Follow the focused link / press the focused button"),
    ("↑ ↓, PgUp PgDn, Space", "Scroll"),
    ("Home / End", "Top / bottom of the page"),
    ("Alt-← / Alt-→, Backspace", "Back / forward"),
    ("F5, Ctrl-R", "Reload"),
    ("Esc", "Stop loading, leave a text field"),
    ("F1", "This help"),
    ("Ctrl-Q, Ctrl-C", "Quit"),
];

const STYLE: &str = r#"
  body { margin: 0; background: #f9f9fb; color: #15141a; font-family: sans-serif; }
  .wrap { max-width: 760px; margin: 0 auto; padding: 32px 24px; }
  h1 { color: #592acb; margin: 0 0 8px 0; }
  .sub { color: #5b5b66; margin-bottom: 32px; }
  form { display: flex; gap: 8px; margin-bottom: 32px; }
  input[type=text] { flex: 1; padding: 0 8px; border: 1px solid #8f8f9d; border-radius: 4px; background: white; color: #15141a; }
  button { padding: 0 16px; background: #0061e0; color: white; border: none; border-radius: 4px; }
  .tiles { display: grid; grid-template-columns: repeat(auto-fill, minmax(200px, 1fr)); gap: 16px; }
  .tile { display: block; background: white; border: 1px solid #e0e0e6; border-radius: 8px; padding: 16px; color: #15141a; text-decoration: none; }
  .tile b { color: #0061e0; display: block; }
  .tile span { color: #5b5b66; }
  table { border-collapse: collapse; width: 100%; background: white; }
  td { padding: 0 8px; border-bottom: 1px solid #e0e0e6; }
  td:first-child { color: #0061e0; font-weight: bold; white-space: nowrap; }
  code { background: #ededf0; padding: 0 4px; }
  .err { background: #fff4f4; border-left: 4px solid #d70022; padding: 16px; }
  .foot { color: #8f8f9d; margin-top: 32px; }
"#;

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><title>{title}</title><style>{STYLE}</style></head><body><div class=wrap>{body}</div></body></html>"
    )
}

pub fn home() -> String {
    let tiles = [
        ("Wikipedia", "The free encyclopedia", "https://en.wikipedia.org/"),
        ("Hacker News", "Tech news and discussion", "https://news.ycombinator.com/"),
        ("Lobsters", "Computing-focused links", "https://lobste.rs/"),
        ("NPR Text", "Text-only news", "https://text.npr.org/"),
        ("DuckDuckGo Lite", "Private web search", "https://lite.duckduckgo.com/lite/"),
        ("Lineal on GitHub", "Source and releases", "https://github.com/Zexolver/lineal-browser"),
    ];
    let tiles: String =
        tiles.iter().map(|(t, d, u)| format!("<a class=tile href=\"{u}\"><b>{t}</b><span>{d}</span></a>")).collect();
    page(
        "New Tab",
        &format!(
            r#"<h1>Lineal</h1><div class=sub>A lightweight web browser for your terminal.</div>
<form action="https://lineal.invalid/search"><input type=text name=q placeholder="Search the web or type a URL"><button>Search</button></form>
<div class=tiles>{tiles}</div>
<div class=foot>Lineal {} · press F1 for keys</div>"#,
            env!("CARGO_PKG_VERSION")
        ),
    )
}

pub fn help() -> String {
    let rows: String =
        KEYS.iter().map(|(k, v)| format!("<tr><td>{}</td><td>{}</td></tr>", escape(k), escape(v))).collect();
    page(
        "Help",
        &format!(
            r#"<h1>Keys</h1><table>{rows}</table>
<h2>Settings</h2>
<p><code>LINEAL_SEARCH</code>: search URL with <code>%s</code> for the query (default DuckDuckGo Lite).<br>
<code>LINEAL_HOME</code>: start page when no URL is given.</p>
<p><a href="about:home">Back to the start page</a></p>"#
        ),
    )
}

pub fn error(url: &str, message: &str) -> String {
    page(
        "Problem loading page",
        &format!(
            r#"<h1>Hmm. We can't load this page.</h1><div class=err><b>{}</b><br>{}</div>
<p>Press F5 to try again, or Alt-← to go back.</p>"#,
            escape(url),
            escape(message)
        ),
    )
}

/// Plain text documents are shown like browsers do: preformatted, in a monospace font.
pub fn plain_text(text: &str) -> String {
    format!(
        "<!doctype html><html><body><pre style=\"white-space: pre-wrap; margin: 8px\">{}</pre></body></html>",
        escape(text)
    )
}

pub enum About {
    Page(Document),
    Redirect(Url),
}

/// Internal pages live under `about:`. Forms can't post to `about:` URLs, so the start page's
/// search box uses `https://lineal.invalid/search` instead (`.invalid` never resolves), which is
/// intercepted here too.
pub fn is_internal(url: &Url) -> bool {
    url.scheme() == "about" || url.host_str() == Some("lineal.invalid")
}

/// Resolves an internal URL. `about:search?q=…` goes to the search engine (or the URL typed).
pub fn about(url: &Url, search: &str) -> About {
    if url.path() == "search" || url.path() == "/search" {
        let q = url.query_pairs().find(|(k, _)| k == "q").map(|(_, v)| v.into_owned()).unwrap_or_default();
        return About::Redirect(crate::app::resolve_input(&q, search).url);
    }
    let text = match url.path() {
        "home" | "newtab" => home(),
        "help" => help(),
        "blank" => String::new(),
        other => error(url.as_str(), &format!("There is no page called about:{other}.")),
    };
    About::Page(Document { url: url.clone(), mime: "text/html".into(), text, status: 200 })
}

pub const DEFAULT_SEARCH: &str = "https://lite.duckduckgo.com/lite/?q=%s";

pub fn search_url(template: &str, query: &str) -> String {
    let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    template.replace("%s", &q)
}
