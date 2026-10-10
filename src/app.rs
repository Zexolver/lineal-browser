//! Browser state: the current page, history, the address bar, and what keys and the mouse do.

use std::rc::Rc;
use std::time::Instant;

use blitz_traits::navigation::NavigationOptions;
use blitz_traits::net::{Body, EntryValue};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use keyboard_types::{Key, Modifiers};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::editor::{Edit, Editor};
use crate::engine::{Engine, Page};
use crate::net::{self, Document, Method, Outcome, Request};
use crate::pages::{self, About};
use crate::paint::Grid;

/// Keep the sources of this many history entries around each side of the current one.
const CACHED_ENTRIES: usize = 10;

pub enum Msg {
    Loaded {
        generation: u64,
        req: Request,
        push: bool,
        scroll: Option<f64>,
        result: Result<Outcome, String>,
    },
    /// A link was clicked or a form submitted inside the page.
    Navigate(NavigationOptions),
    /// A stylesheet or other resource arrived; the page needs a new layout.
    Redraw,
}

struct Entry {
    req: Request,
    scroll: f64,
    doc: Option<Rc<Document>>,
}

pub enum Mode {
    Normal,
    /// Editing the address bar.
    Address(Editor),
}

pub struct Loading {
    pub url: String,
    pub since: Instant,
    task: tokio::task::AbortHandle,
}

/// Screen columns of the toolbar's clickable parts (set by the UI when drawing).
#[derive(Default, Clone, Copy)]
pub struct Toolbar {
    pub row: u16,
    pub back: (u16, u16),
    pub forward: (u16, u16),
    pub reload: (u16, u16),
    pub address: (u16, u16),
}

pub struct App {
    client: reqwest::Client,
    tx: UnboundedSender<Msg>,
    engine: Engine,
    generation: u64,
    pub page: Option<Page>,
    /// The URL shown in the address bar.
    pub url: Url,
    pub title: String,
    history: Vec<Entry>,
    index: usize,
    pub mode: Mode,
    pub loading: Option<Loading>,
    pub message: Option<(String, Instant)>,
    /// Link under the mouse pointer or focused, shown at the bottom like Firefox does.
    pub hover: Option<Url>,
    pub quit: bool,
    /// Content area on screen: x, y, columns, rows.
    pub viewport: (u16, u16, usize, usize),
    pub toolbar: Toolbar,
    grid: Option<Grid>,
    dirty: bool,
    search: String,
}

impl App {
    pub fn new(client: reqwest::Client, tx: UnboundedSender<Msg>, search: String) -> App {
        let engine = Engine::new(client.clone(), tx.clone());
        App {
            client,
            tx,
            engine,
            generation: 0,
            page: None,
            url: Url::parse("about:blank").unwrap(),
            title: String::new(),
            history: Vec::new(),
            index: 0,
            mode: Mode::Normal,
            loading: None,
            message: None,
            hover: None,
            quit: false,
            viewport: (0, 0, 80, 24),
            toolbar: Toolbar::default(),
            grid: None,
            dirty: true,
            search,
        }
    }

    pub fn message(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
    }

    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.index + 1 < self.history.len()
    }

    /// Sets the content area; re-lays out the page if its size changed.
    pub fn set_viewport(&mut self, x: u16, y: u16, cols: usize, rows: usize) {
        if self.viewport != (x, y, cols, rows) {
            self.viewport = (x, y, cols, rows);
            if let Some(p) = &mut self.page {
                p.resize(cols, rows);
            }
            self.dirty = true;
        }
    }

    /// The painted page, repainting only when something changed.
    pub fn grid(&mut self) -> Option<&Grid> {
        if self.dirty || self.grid.is_none() {
            self.grid = self.page.as_mut().map(|p| p.paint());
            self.dirty = false;
            if let Some(p) = &self.page {
                let t = p.title();
                if !t.is_empty() {
                    self.title = t;
                }
            }
        }
        self.grid.as_ref()
    }

    pub fn scroll_fraction(&self) -> Option<(f64, f64)> {
        let (y, view, total) = self.page.as_ref()?.scroll_info();
        (total > view).then(|| (y / total, (y + view) / total))
    }

    // ---- navigation -----------------------------------------------------------------------

    pub fn navigate(&mut self, req: Request) {
        self.load(req, true, None);
    }

    fn load(&mut self, mut req: Request, push: bool, scroll: Option<f64>) {
        self.mode = Mode::Normal;
        if pages::is_internal(&req.url) {
            match pages::about(&req.url, &self.search) {
                About::Redirect(url) => req = Request::get(url),
                About::Page(doc) => {
                    self.cancel();
                    self.show(req, Rc::new(doc), push, scroll, true);
                    return;
                }
            }
        }
        // Links to another part of the same page just scroll there.
        if push && req.method == Method::Get && self.loading.is_none() && self.page.is_some() {
            let (mut a, mut b) = (req.url.clone(), self.url.clone());
            a.set_fragment(None);
            b.set_fragment(None);
            if a == b
                && let Some(fragment) = req.url.fragment()
            {
                let fragment = fragment.to_string();
                self.save_scroll();
                let doc = self.history.get(self.index).and_then(|e| e.doc.clone());
                self.history.truncate(self.index + 1);
                self.history.push(Entry { req: req.clone(), scroll: 0.0, doc });
                self.index = self.history.len() - 1;
                self.url = req.url.clone();
                if let Some(p) = &mut self.page {
                    p.scroll_to_fragment(&fragment);
                }
                self.dirty = true;
                return;
            }
        }
        if !matches!(req.url.scheme(), "http" | "https" | "file") {
            self.message(format!("Lineal can't open {}: links", req.url.scheme()));
            return;
        }
        self.cancel();
        self.generation += 1;
        let (generation, tx, client, r) = (self.generation, self.tx.clone(), self.client.clone(), req.clone());
        let task = tokio::spawn(async move {
            let result = net::fetch(&client, &r).await;
            let _ = tx.send(Msg::Loaded { generation, req: r, push, scroll, result });
        });
        self.url = req.url.clone();
        self.loading = Some(Loading { url: req.url.to_string(), since: Instant::now(), task: task.abort_handle() });
    }

    fn cancel(&mut self) {
        if let Some(l) = self.loading.take() {
            l.task.abort();
            // Show the address of the page that's actually on screen again.
            if let Some(e) = self.history.get(self.index) {
                self.url = e.req.url.clone();
            }
        }
        self.generation += 1;
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Loaded { generation, req, push, scroll, result } => {
                if generation != self.generation {
                    return;
                }
                self.loading = None;
                match result {
                    Ok(Outcome::Doc(doc)) => self.show(req, Rc::new(doc), push, scroll, true),
                    Ok(Outcome::Downloaded { path, bytes }) => {
                        if let Some(e) = self.history.get(self.index) {
                            self.url = e.req.url.clone();
                        }
                        self.message(format!("Downloaded {} ({})", path.display(), human_size(bytes)));
                    }
                    Err(e) => {
                        let text = pages::error(req.url.as_str(), &e);
                        let doc = Document { url: req.url.clone(), mime: "text/html".into(), text, status: 0 };
                        self.show(req, Rc::new(doc), push, None, false);
                    }
                }
            }
            Msg::Navigate(options) => {
                let req = request_from(options);
                self.navigate(req);
            }
            Msg::Redraw => self.dirty = true,
        }
    }

    fn show(&mut self, req: Request, doc: Rc<Document>, push: bool, scroll: Option<f64>, cache: bool) {
        let cached = cache.then(|| doc.clone());
        if push || self.history.is_empty() {
            self.save_scroll();
            if !self.history.is_empty() {
                self.history.truncate(self.index + 1);
            }
            self.history.push(Entry { req: req.clone(), scroll: 0.0, doc: cached });
            self.index = self.history.len() - 1;
        } else if let Some(e) = self.history.get_mut(self.index) {
            e.req = req.clone();
            e.doc = cached;
        }
        for (i, e) in self.history.iter_mut().enumerate() {
            if i.abs_diff(self.index) > CACHED_ENTRIES {
                e.doc = None;
            }
        }
        self.open(&doc, scroll);
    }

    /// Builds the page for a document.
    fn open(&mut self, doc: &Document, scroll: Option<f64>) {
        let html = if doc.is_html() { doc.text.clone() } else { pages::plain_text(&doc.text) };
        let (_, _, cols, rows) = self.viewport;
        let mut page = self.engine.page(&html, &doc.url, cols, rows);
        if let Some(y) = scroll {
            page.scroll_to(y);
        }
        self.url = doc.url.clone();
        self.title = page.title();
        if self.title.is_empty() {
            self.title = doc.url.host_str().unwrap_or(doc.url.as_str()).to_string();
        }
        if doc.status >= 400 {
            self.message(format!("The server answered with error {}", doc.status));
        }
        self.page = Some(page);
        self.hover = None;
        self.dirty = true;
    }

    fn save_scroll(&mut self) {
        let y = self.page.as_ref().map(|p| p.scroll_info().0);
        if let (Some(e), Some(y)) = (self.history.get_mut(self.index), y) {
            e.scroll = y;
        }
    }

    fn go(&mut self, delta: isize) {
        let Some(new) = self.index.checked_add_signed(delta).filter(|&i| i < self.history.len()) else { return };
        self.save_scroll();
        self.index = new;
        let (req, scroll, doc) = {
            let e = &self.history[new];
            (e.req.clone(), e.scroll, e.doc.clone())
        };
        match doc {
            Some(doc) => {
                self.cancel();
                self.open(&doc, Some(scroll));
                self.url = req.url;
            }
            None => self.load(req, false, Some(scroll)),
        }
    }

    fn reload(&mut self) {
        let Some(e) = self.history.get(self.index) else { return };
        let (req, scroll) = (e.req.clone(), self.page.as_ref().map(|p| p.scroll_info().0));
        self.load(req, false, scroll);
    }

    pub fn resolve_input(&self, input: &str) -> Request {
        resolve_input(input, &self.search)
    }

    fn edit_address(&mut self) {
        let text = if self.url.scheme() == "about" { String::new() } else { self.url.to_string() };
        self.mode = Mode::Address(Editor::selected(&text));
    }

    // ---- input ----------------------------------------------------------------------------

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(text) => match &mut self.mode {
                Mode::Address(e) => e.paste(&text),
                Mode::Normal => {
                    if let Some(p) = self.page.as_mut().filter(|p| p.wants_keys()) {
                        for c in text.chars().filter(|c| !c.is_control()) {
                            p.key(Key::Character(c.to_string()), Some(&c.to_string()), Modifiers::empty());
                        }
                        self.dirty = true;
                    }
                }
            },
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
    }

    /// Converts screen coordinates to a cell inside the page.
    fn page_cell(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let (vx, vy, cols, rows) = self.viewport;
        let (col, row) = (x.checked_sub(vx)? as usize, y.checked_sub(vy)? as usize);
        (col < cols && row < rows).then_some((col, row))
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let cell = self.page_cell(m.column, m.row);
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let lines = if m.kind == MouseEventKind::ScrollDown { 3.0 } else { -3.0 };
                if let Some(p) = &mut self.page {
                    match cell {
                        Some((c, r)) => p.wheel(c, r, lines),
                        None => p.scroll_by(lines),
                    }
                    self.dirty = true;
                }
            }
            MouseEventKind::Moved => {
                if let (Some((c, r)), Some(p)) = (cell, &mut self.page) {
                    p.mouse_move(c, r);
                    let link = p.link_at(c, r);
                    if link != self.hover {
                        self.hover = link;
                    }
                    self.dirty = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((c, r)) = cell {
                    self.mode = Mode::Normal;
                    if let Some(p) = &mut self.page {
                        p.click(c, r);
                        self.dirty = true;
                    }
                    return;
                }
                let t = self.toolbar;
                let inside = |(a, b): (u16, u16)| m.column >= a && m.column < b;
                if m.row == t.row {
                    if inside(t.back) {
                        self.go(-1);
                    } else if inside(t.forward) {
                        self.go(1);
                    } else if inside(t.reload) {
                        if self.loading.is_some() {
                            self.cancel();
                        } else {
                            self.reload();
                        }
                    } else if inside(t.address) && matches!(self.mode, Mode::Normal) {
                        self.edit_address();
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                // No tabs yet: middle-click just opens the link here.
                if let Some(url) = cell.and_then(|(c, r)| self.page.as_ref()?.link_at(c, r)) {
                    self.navigate(Request::get(url));
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Browser-wide shortcuts work everywhere.
        match key.code {
            KeyCode::Char('q') if ctrl => return self.quit = true,
            KeyCode::Char('l') if ctrl => return self.edit_address(),
            KeyCode::Char('d') if alt => return self.edit_address(),
            KeyCode::F(6) => return self.edit_address(),
            KeyCode::Char('r') if ctrl => return self.reload(),
            KeyCode::F(5) => return self.reload(),
            KeyCode::Left if alt => return self.go(-1),
            KeyCode::Right if alt => return self.go(1),
            KeyCode::F(1) => return self.navigate(Request::get(Url::parse("about:help").unwrap())),
            KeyCode::Char('f') if ctrl => return self.message("Find in page is coming in a later build"),
            KeyCode::Char('t') if ctrl => return self.message("Tabs are coming in a later build"),
            _ => {}
        }
        if let Mode::Address(editor) = &mut self.mode {
            match editor.key(key) {
                Edit::Submit => {
                    let text = editor.text();
                    let req = self.resolve_input(&text);
                    self.mode = Mode::Normal;
                    self.navigate(req);
                }
                Edit::Cancel => self.mode = Mode::Normal,
                _ => {}
            }
            return;
        }
        let Some(page) = &mut self.page else {
            if key.code == KeyCode::Char('c') && ctrl {
                self.quit = true;
            }
            return;
        };
        self.dirty = true;
        // A focused text field gets the keys, like in any browser.
        if page.wants_keys() {
            match key.code {
                KeyCode::Esc => page.blur(),
                KeyCode::Tab => {
                    page.focus_next(true);
                }
                KeyCode::BackTab => {
                    page.focus_next(false);
                }
                code => {
                    if let Some((k, text)) = to_key(code) {
                        let mut mods = Modifiers::empty();
                        if ctrl {
                            mods |= Modifiers::CONTROL;
                        }
                        if alt {
                            mods |= Modifiers::ALT;
                        }
                        if key.modifiers.contains(KeyModifiers::SHIFT) {
                            mods |= Modifiers::SHIFT;
                        }
                        page.key(k, if ctrl { None } else { text.as_deref() }, mods);
                    }
                }
            }
            return;
        }
        let rows = self.viewport.3 as f64;
        let moves_focus = matches!(key.code, KeyCode::Tab | KeyCode::BackTab);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Tab => {
                page.focus_next(true);
            }
            KeyCode::BackTab => {
                page.focus_next(false);
            }
            KeyCode::Enter => {
                if page.has_focus() {
                    page.activate_focused();
                } else {
                    page.focus_next(true);
                }
            }
            KeyCode::Down => page.scroll_by(2.0),
            KeyCode::Up => page.scroll_by(-2.0),
            KeyCode::PageDown | KeyCode::Char(' ') => page.scroll_by(rows - 2.0),
            KeyCode::PageUp => page.scroll_by(-(rows - 2.0)),
            KeyCode::Home => page.scroll_to(0.0),
            KeyCode::End => page.scroll_to(f64::MAX),
            KeyCode::Backspace => self.go(-1),
            KeyCode::Esc => {
                if self.loading.is_some() {
                    self.cancel();
                    self.message("Stopped");
                } else {
                    page.blur();
                }
            }
            _ => {}
        }
        // Like Firefox, show where a keyboard-focused link goes.
        if moves_focus {
            self.hover = self.page.as_ref().and_then(|p| p.focused_link());
        }
    }
}

/// crossterm key -> W3C key (what Blitz's text fields understand).
fn to_key(code: KeyCode) -> Option<(Key, Option<String>)> {
    Some(match code {
        KeyCode::Char(c) => (Key::Character(c.to_string()), Some(c.to_string())),
        KeyCode::Enter => (Key::Enter, None),
        KeyCode::Backspace => (Key::Backspace, None),
        KeyCode::Delete => (Key::Delete, None),
        KeyCode::Left => (Key::ArrowLeft, None),
        KeyCode::Right => (Key::ArrowRight, None),
        KeyCode::Up => (Key::ArrowUp, None),
        KeyCode::Down => (Key::ArrowDown, None),
        KeyCode::Home => (Key::Home, None),
        KeyCode::End => (Key::End, None),
        KeyCode::PageUp => (Key::PageUp, None),
        KeyCode::PageDown => (Key::PageDown, None),
        _ => return None,
    })
}

/// A navigation requested by the page (link or form) -> our request type.
fn request_from(options: NavigationOptions) -> Request {
    let post = options.method == blitz_traits::net::Method::POST;
    let body = match options.document_resource {
        Body::Form(form) => {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for entry in form.iter() {
                if let EntryValue::String(v) = &entry.value {
                    ser.append_pair(&entry.name, v);
                }
            }
            Some(ser.finish())
        }
        Body::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Body::Empty => None,
    };
    if post {
        Request { url: options.url, method: Method::Post, body: Some(body.unwrap_or_default()) }
    } else {
        Request::get(options.url)
    }
}

pub fn resolve_input(input: &str, search: &str) -> Request {
    let s = input.trim();
    let url = if s.is_empty() {
        Url::parse("about:home").ok()
    } else if s.contains("://") || s.starts_with("about:") || s.starts_with("file:") {
        Url::parse(s).ok()
    } else if let Some(path) = local_path(s) {
        Url::from_file_path(path).ok()
    } else if !s.contains(char::is_whitespace) && looks_like_host(s) {
        let host = s.split(['/', ':', '?', '#']).next().unwrap_or("");
        let local = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok();
        Url::parse(&format!("{}://{s}", if local { "http" } else { "https" })).ok()
    } else {
        None
    };
    Request::get(url.unwrap_or_else(|| Url::parse(&pages::search_url(search, s)).expect("bad search URL template")))
}

fn looks_like_host(s: &str) -> bool {
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    let name =
        host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
    name == "localhost"
        || name.parse::<std::net::IpAddr>().is_ok()
        || (name.contains('.')
            && !name.starts_with('.')
            && !name.ends_with('.')
            && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.')
            && name.rsplit('.').next().is_some_and(|tld| tld.chars().any(|c| c.is_alphabetic())))
}

fn local_path(s: &str) -> Option<std::path::PathBuf> {
    let expanded = match s.strip_prefix("~/") {
        Some(rest) => {
            std::path::PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?).join(rest)
        }
        None => std::path::PathBuf::from(s),
    };
    let explicit = s.starts_with(['/', '.', '~', '\\']) || s.get(1..3) == Some(":\\");
    if explicit || expanded.exists() { expanded.canonicalize().ok() } else { None }
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_resolution() {
        let r = |s| resolve_input(s, pages::DEFAULT_SEARCH).url.to_string();
        assert_eq!(r("example.org"), "https://example.org/");
        assert_eq!(r("example.org/a?b"), "https://example.org/a?b");
        assert_eq!(r("localhost:8080"), "http://localhost:8080/");
        assert_eq!(r("http://x.y"), "http://x.y/");
        assert_eq!(r("rust tui"), "https://lite.duckduckgo.com/lite/?q=rust+tui");
        assert_eq!(r("ratatui"), "https://lite.duckduckgo.com/lite/?q=ratatui");
        assert_eq!(r(""), "about:home");
    }
}
