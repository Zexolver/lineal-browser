//! Built-in `about:` pages, rendered by the same engine as everything else.

use crate::net::escape;

pub const KEYS: &[(&str, &str)] = &[
    ("o", "open a URL or search"),
    ("O", "edit the current URL"),
    ("Tab / Shift-Tab", "next / previous link or field"),
    ("Enter", "follow link / activate field"),
    ("f", "link hints: type the label to follow"),
    ("j k / ↓ ↑", "scroll a line"),
    ("Space / b, PgDn / PgUp", "scroll a page"),
    ("Ctrl-d / Ctrl-u", "scroll half a page"),
    ("g / G, Home / End", "top / bottom"),
    ("H / L, Backspace, Alt-← / Alt-→", "back / forward"),
    ("r / F5", "reload"),
    ("/  then  n / N", "find in page, next / previous match"),
    ("y / Y", "copy page URL / focused link URL (OSC 52)"),
    ("\\", "toggle page source"),
    ("Esc", "stop loading, clear focus and search"),
    ("?", "this help"),
    ("q / Ctrl-c", "quit"),
];

pub fn home() -> String {
    format!(
        r#"<title>Lineal</title>
<h1>Lineal</h1>
<p>A lightweight terminal web browser.</p>
<form action="about:search"><input name="q" size="40" placeholder="Search the web"> <input type="submit" value="Search"></form>
<p>Press <kbd>o</kbd> to open a URL or search, <kbd>Tab</kbd> to move between links, <kbd>f</kbd> for link hints and <kbd>?</kbd> for help.</p>
<h2>Try</h2>
<ul>
<li><a href="https://lite.duckduckgo.com/lite/">DuckDuckGo Lite</a></li>
<li><a href="https://en.wikipedia.org/wiki/Special:Random">A random Wikipedia article</a></li>
<li><a href="https://news.ycombinator.com/">Hacker News</a></li>
<li><a href="https://lobste.rs/">Lobsters</a></li>
<li><a href="https://text.npr.org/">NPR text-only news</a></li>
<li><a href="https://github.com/Zexolver/lineal-browser">Lineal on GitHub</a></li>
</ul>
<p>Lineal {}</p>"#,
        env!("CARGO_PKG_VERSION")
    )
}

pub fn help() -> String {
    let mut rows = String::new();
    for (k, v) in KEYS {
        rows += &format!("<tr><td><kbd>{}</kbd><td>{}</tr>", escape(k), escape(v));
    }
    format!(
        r#"<title>Help</title><h1>Keys</h1><table>{rows}</table>
<h2>In text boxes</h2>
<p>Enter submits, Tab moves to the next field, Esc cancels. Ctrl-a/e/u/k/w work like in a shell.</p>
<h2>Mouse</h2>
<p>Click links and fields, scroll with the wheel.</p>
<h2>Environment</h2>
<ul>
<li><code>LINEAL_SEARCH</code>: search URL with <code>%s</code> for the query (default DuckDuckGo Lite).</li>
<li><code>LINEAL_HOME</code>: start page when no URL is given.</li>
<li><code>NO_COLOR</code>: disable colors.</li>
</ul>
<p><a href="about:home">Back to the start page</a></p>"#
    )
}

pub fn error(url: &str, message: &str) -> String {
    format!(
        r#"<title>Error</title><h1>Could not load the page</h1><p><code>{}</code></p><pre>{}</pre><p>Press <kbd>r</kbd> to retry or <kbd>H</kbd> to go back.</p>"#,
        escape(url),
        escape(message)
    )
}

pub enum About {
    Page(crate::net::Document),
    Redirect(url::Url),
}

/// Resolves an `about:` URL. `about:search?q=…` redirects to the search engine.
pub fn about(url: &url::Url, search: &str) -> About {
    if url.path() == "search" {
        let q = url.query_pairs().find(|(k, _)| k == "q").map(|(_, v)| v.into_owned()).unwrap_or_default();
        if let Ok(u) = url::Url::parse(&search_url(search, &q)) {
            return About::Redirect(u);
        }
    }
    let text = match url.path() {
        "home" => home(),
        "help" => help(),
        "blank" => String::new(),
        other => error(url.as_str(), &format!("unknown page about:{other}")),
    };
    About::Page(crate::net::Document { url: url.clone(), mime: "text/html".into(), text, status: 200, hidden: Vec::new() })
}

pub const DEFAULT_SEARCH: &str = "https://lite.duckduckgo.com/lite/?q=%s";

pub fn search_url(template: &str, query: &str) -> String {
    let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    template.replace("%s", &q)
}
