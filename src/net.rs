//! Networking: HTTP(S) via reqwest + rustls (ring), local files, charset decoding and downloads.

use std::path::{Path, PathBuf};
use std::time::Duration;

use encoding_rs::{Encoding, UTF_8};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap, HeaderValue};
use tokio::io::AsyncWriteExt;
use url::Url;

pub const USER_AGENT: &str = concat!(
    "Lineal/",
    env!("CARGO_PKG_VERSION"),
    " (terminal browser; +https://github.com/Zexolver/lineal-browser)"
);

/// Documents bigger than this are cut off; nobody reads 16 MiB of text in a terminal.
const MAX_DOCUMENT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub url: Url,
    pub method: Method,
    pub body: Option<String>,
}

impl Request {
    pub fn get(url: Url) -> Self {
        Self { url, method: Method::Get, body: None }
    }
}

/// A fetched text document.
#[derive(Clone, Debug)]
pub struct Document {
    /// Final URL after redirects.
    pub url: Url,
    /// Lowercase MIME essence, e.g. `text/html`.
    pub mime: String,
    pub text: String,
    pub status: u16,
}

impl Document {
    pub fn is_html(&self) -> bool {
        matches!(self.mime.as_str(), "text/html" | "application/xhtml+xml" | "")
    }
}

pub enum Outcome {
    Doc(Document),
    Downloaded { path: PathBuf, bytes: u64 },
}

pub fn client() -> reqwest::Result<reqwest::Client> {
    // Pure-Rust-friendly crypto (ring) instead of aws-lc, which needs cmake and is painful to cross-compile.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8"),
    );
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_str(&accept_language()).unwrap_or(HeaderValue::from_static("en")));
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()
}

/// `LANG=de_DE.UTF-8` -> `de-DE,de;q=0.9,en;q=0.5`.
fn accept_language() -> String {
    let lang = std::env::var("LANG").unwrap_or_default();
    let tag = lang.split(['.', '@']).next().unwrap_or("").replace('_', "-");
    if tag.is_empty() || tag == "C" || tag == "POSIX" {
        return "en".into();
    }
    let primary = tag.split('-').next().unwrap_or("en");
    if primary == "en" { format!("{tag},en;q=0.9") } else { format!("{tag},{primary};q=0.9,en;q=0.5") }
}

pub async fn fetch(client: &reqwest::Client, req: &Request) -> Result<Outcome, String> {
    match req.url.scheme() {
        "http" | "https" => fetch_http(client, req).await,
        "file" => fetch_file(&req.url).await,
        other => Err(format!("unsupported URL scheme `{other}:`")),
    }
}

async fn fetch_http(client: &reqwest::Client, req: &Request) -> Result<Outcome, String> {
    let builder = match req.method {
        Method::Get => client.get(req.url.clone()),
        Method::Post => client
            .post(req.url.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(req.body.clone().unwrap_or_default()),
    };
    let mut resp = builder.send().await.map_err(describe_error)?;
    let url = resp.url().clone();
    let status = resp.status().as_u16();
    let header = |name| resp.headers().get(name).and_then(|v: &HeaderValue| v.to_str().ok()).map(str::to_owned);
    let content_type = header(CONTENT_TYPE).unwrap_or_default();
    let disposition = header(CONTENT_DISPOSITION).unwrap_or_default();
    let (mime, charset) = parse_content_type(&content_type);

    let attachment = disposition.trim_start().to_ascii_lowercase().starts_with("attachment");
    if attachment || !is_textual(&mime) {
        let name = disposition_filename(&disposition).unwrap_or_else(|| url_filename(&url));
        let path = unique_path(&download_dir(), &name);
        let mut file = tokio::fs::File::create(&path).await.map_err(|e| format!("{}: {e}", path.display()))?;
        let mut bytes = 0u64;
        while let Some(chunk) = resp.chunk().await.map_err(describe_error)? {
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            bytes += chunk.len() as u64;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        return Ok(Outcome::Downloaded { path, bytes });
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(describe_error)? {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_DOCUMENT {
            body.truncate(MAX_DOCUMENT);
            break;
        }
    }
    let html = mime == "text/html" || mime == "application/xhtml+xml" || mime.is_empty();
    let text = decode(&body, charset.as_deref(), html);
    Ok(Outcome::Doc(Document { url, mime, text, status }))
}

async fn fetch_file(url: &Url) -> Result<Outcome, String> {
    let path = url.to_file_path().map_err(|_| format!("bad file URL: {url}"))?;
    let meta = tokio::fs::metadata(&path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_dir() {
        return Ok(Outcome::Doc(Document {
            url: url.clone(),
            mime: "text/html".into(),
            text: directory_listing(&path).await?,
            status: 200,
        }));
    }
    let bytes = tokio::fs::read(&path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "html" | "htm" | "xhtml" => "text/html",
        _ => "text/plain",
    };
    Ok(Outcome::Doc(Document { url: url.clone(), mime: mime.into(), text: decode(&bytes, None, mime == "text/html"), status: 200 }))
}

async fn directory_listing(path: &Path) -> Result<String, String> {
    let mut dir = tokio::fs::read_dir(path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    let mut entries = Vec::new();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push((!is_dir, name.to_lowercase(), name, is_dir));
    }
    entries.sort();
    let title = escape(&path.display().to_string());
    let mut html = format!("<title>{title}</title><h1>Index of {title}</h1><ul><li><a href=\"../\">../</a></li>");
    for (_, _, name, is_dir) in entries {
        let slash = if is_dir { "/" } else { "" };
        let href: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>().replace('+', "%20");
        html += &format!("<li><a href=\"{href}{slash}\">{}{slash}</a></li>", escape(&name));
    }
    Ok(html + "</ul>")
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn describe_error(e: reqwest::Error) -> String {
    use std::error::Error;
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        msg = format!("{msg}: {s}");
        source = s.source();
    }
    msg
}

/// Returns (lowercase MIME essence, charset parameter).
fn parse_content_type(value: &str) -> (String, Option<String>) {
    let mut parts = value.split(';');
    let mime = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let charset = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches('"').to_string())
    });
    (mime, charset)
}

fn is_textual(mime: &str) -> bool {
    mime.is_empty()
        || mime.starts_with("text/")
        || mime.ends_with("+xml")
        || mime.ends_with("+json")
        || matches!(mime, "application/xml" | "application/json" | "application/javascript" | "application/x-javascript")
}

/// Decodes bytes using (in order) a BOM, the HTTP charset, a `<meta>` charset, then UTF-8.
pub fn decode(bytes: &[u8], header_charset: Option<&str>, html: bool) -> String {
    let encoding = Encoding::for_bom(bytes)
        .map(|(enc, _)| enc)
        .or_else(|| header_charset.and_then(|c| Encoding::for_label(c.as_bytes())))
        .or_else(|| if html { sniff_meta_charset(bytes) } else { None })
        .unwrap_or(UTF_8);
    encoding.decode(bytes).0.into_owned()
}

fn sniff_meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = &bytes[..bytes.len().min(2048)];
    let lower: Vec<u8> = head.iter().map(u8::to_ascii_lowercase).collect();
    let pos = lower.windows(8).position(|w| w == b"charset=")? + 8;
    let rest = &head[pos..];
    let rest = rest.strip_prefix(b"\"").or_else(|| rest.strip_prefix(b"'")).unwrap_or(rest);
    let end = rest.iter().position(|b| !(b.is_ascii_alphanumeric() || b"-_:.".contains(b))).unwrap_or(rest.len());
    let enc = Encoding::for_label(&rest[..end])?;
    // A UTF-16 label inside an ASCII-compatible byte stream is a lie (per the HTML spec).
    Some(if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE { UTF_8 } else { enc })
}

fn disposition_filename(value: &str) -> Option<String> {
    value.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        let k = k.trim().to_ascii_lowercase();
        if k == "filename*" {
            let v = v.trim().trim_matches('"');
            let encoded = v.rsplit("''").next()?;
            let bytes: Vec<u8> = url::form_urlencoded::parse(format!("x={encoded}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned().into_bytes())?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        } else if k == "filename" {
            Some(v.trim().trim_matches('"').to_string())
        } else {
            None
        }
    })
    .map(|n| sanitize_filename(&n))
    .filter(|n| !n.is_empty())
}

fn url_filename(url: &Url) -> String {
    let name = url.path_segments().and_then(|mut s| s.next_back().map(str::to_owned)).unwrap_or_default();
    let name = sanitize_filename(&name);
    if name.is_empty() { "download".into() } else { name }
}

fn sanitize_filename(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect::<String>()
        .trim_matches(['.', ' '])
        .to_string()
}

/// `~/Downloads` if it exists, otherwise the current directory.
pub fn download_dir() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(home) = home {
        let dl = PathBuf::from(home).join("Downloads");
        if dl.is_dir() {
            return dl;
        }
    }
    PathBuf::from(".")
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..).map(|i| dir.join(format!("{stem}.{i}{ext}"))).find(|p| !p.exists()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_parsing() {
        assert_eq!(parse_content_type("Text/HTML; charset=\"ISO-8859-1\""), ("text/html".into(), Some("ISO-8859-1".into())));
        assert_eq!(parse_content_type(""), (String::new(), None));
    }

    #[test]
    fn meta_charset_is_sniffed() {
        let html = b"<html><head><meta charset=\"windows-1252\"></head><body>caf\xe9</body>";
        assert!(decode(html, None, true).contains("café"));
        assert!(decode("café".as_bytes(), None, true).contains("café"));
    }

    #[test]
    fn filenames() {
        assert_eq!(disposition_filename("attachment; filename=\"../evil.txt\"").as_deref(), Some("evil.txt"));
        assert_eq!(disposition_filename("attachment; filename*=UTF-8''na%C3%AFve.pdf").as_deref(), Some("naïve.pdf"));
        assert_eq!(url_filename(&Url::parse("https://x.org/a/b.zip?x=1").unwrap()), "b.zip");
        assert_eq!(url_filename(&Url::parse("https://x.org/").unwrap()), "download");
    }
}
