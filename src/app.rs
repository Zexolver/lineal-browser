//! Browser state: the current page, history, input modes and everything keys and the mouse do.

use std::io::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc::UnboundedSender;
use unicode_width::UnicodeWidthChar;
use url::Url;

use crate::dom::Dom;
use crate::editor::{Edit, Editor};
use crate::forms::Kind;
use crate::layout::{Layout, Target};
use crate::net::{self, Document, Method, Outcome, Request};
use crate::pages::{self, About};

/// Keep parsed sources of this many history entries around each side of the current one.
const CACHED_ENTRIES: usize = 15;
/// Follow at most this many `<meta refresh>` redirects in a row.
const MAX_REFRESHES: u32 = 5;
const HINT_KEYS: &str = "asdfghjklqwertyuiopzxcvbnm";

pub enum Msg {
    Loaded { generation: u64, req: Request, push: bool, restore: Option<(usize, Option<usize>)>, result: Result<Outcome, String> },
    Refresh { generation: u64, url: Url },
}

pub enum Content {
    Html(Dom),
    Text,
}

pub struct Page {
    pub req: Request,
    pub doc: Rc<Document>,
    pub title: String,
    pub content: Content,
    pub layout: Layout,
    pub width: usize,
    pub scroll: usize,
    /// Focused target (index into `layout.targets`).
    pub focus: Option<usize>,
    pub source: bool,
}

impl Page {
    fn new(req: Request, doc: Rc<Document>, source: bool, width: usize) -> Page {
        let content = if doc.is_html() && !source { Content::Html(Dom::parse(&doc.text, &doc.url, &doc.hidden)) } else { Content::Text };
        let title = match &content {
            Content::Html(d) if !d.title.is_empty() => d.title.clone(),
            _ => doc.url.to_string(),
        };
        let mut page = Page { req, doc, title, content, layout: Layout::default(), width, scroll: 0, focus: None, source };
        page.layout = page.compute_layout();
        page
    }

    fn compute_layout(&self) -> Layout {
        match &self.content {
            Content::Html(d) => d.layout(self.width, false),
            Content::Text => Layout::text(&self.doc.text, self.width),
        }
    }

    /// Re-lays out after a width change, keeping roughly the same place in the page.
    fn set_width(&mut self, width: usize) {
        if width == self.width {
            return;
        }
        let old = self.layout.lines.len().max(1);
        self.width = width;
        self.layout = self.compute_layout();
        self.scroll = self.scroll * self.layout.lines.len() / old;
    }

    /// Re-lays out after form state changed.
    fn refresh(&mut self) {
        self.layout = self.compute_layout();
    }

    pub fn url(&self) -> &Url {
        &self.doc.url
    }
}

struct Entry {
    req: Request,
    scroll: usize,
    focus: Option<usize>,
    doc: Option<Rc<Document>>,
}

pub struct Find {
    pub query: String,
    /// (line, column, width)
    pub matches: Vec<(usize, usize, usize)>,
    pub current: usize,
}

pub struct Hint {
    pub label: String,
    pub target: usize,
    pub line: usize,
    pub col: usize,
}

pub enum Mode {
    Normal,
    Url(Editor),
    Find(Editor),
    Edit { field: usize, editor: Editor },
    Select { field: usize, cursor: usize },
    Hints { hints: Vec<Hint>, typed: String },
}

pub struct Loading {
    pub url: String,
    pub since: Instant,
    task: tokio::task::AbortHandle,
}

pub struct App {
    client: reqwest::Client,
    tx: UnboundedSender<Msg>,
    generation: u64,
    pub page: Page,
    history: Vec<Entry>,
    index: usize,
    pub mode: Mode,
    pub loading: Option<Loading>,
    pub message: Option<(String, Instant)>,
    pub find: Option<Find>,
    pub quit: bool,
    /// Rows of the content viewport.
    pub height: usize,
    /// Screen position of the content viewport's top-left corner.
    pub origin: (u16, u16),
    search: String,
    refreshes: u32,
}

impl App {
    pub fn new(client: reqwest::Client, tx: UnboundedSender<Msg>, search: String) -> App {
        let blank = Rc::new(Document {
            url: Url::parse("about:blank").unwrap(),
            mime: "text/html".into(),
            text: String::new(),
            status: 200,
            hidden: Vec::new(),
        });
        App {
            client,
            tx,
            generation: 0,
            page: Page::new(Request::get(blank.url.clone()), blank, false, 80),
            history: Vec::new(),
            index: 0,
            mode: Mode::Normal,
            loading: None,
            message: None,
            find: None,
            quit: false,
            height: 24,
            origin: (0, 0),
            search,
            refreshes: 0,
        }
    }

    pub fn set_viewport(&mut self, x: u16, y: u16, width: usize, height: usize) {
        self.origin = (x, y);
        self.height = height.max(1);
        if width != self.page.width {
            self.page.set_width(width);
            self.rerun_find();
            self.clamp_scroll();
        }
    }

    pub fn message(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
    }

    // ---- navigation -------------------------------------------------------

    pub fn navigate(&mut self, req: Request, push: bool) {
        self.load(req, push, None);
    }

    fn load(&mut self, mut req: Request, push: bool, restore: Option<(usize, Option<usize>)>) {
        if req.url.scheme() == "about" {
            match pages::about(&req.url, &self.search) {
                About::Redirect(url) => req = Request::get(url),
                About::Page(doc) => {
                    self.cancel();
                    self.show(req, Rc::new(doc), push, restore, true);
                    return;
                }
            }
        }
        // Same-document fragment links just scroll.
        if push && req.method == Method::Get && req.url.fragment().is_some() && self.loading.is_none() {
            let (mut a, mut b) = (req.url.clone(), self.page.url().clone());
            a.set_fragment(None);
            b.set_fragment(None);
            if a == b && self.page.req.method == Method::Get {
                let mut doc = (*self.page.doc).clone();
                doc.url = req.url.clone();
                self.save_position();
                self.history.truncate(self.index + 1);
                self.history.push(Entry { req: req.clone(), scroll: 0, focus: None, doc: Some(Rc::new(doc.clone())) });
                self.index = self.history.len() - 1;
                self.page.req = req;
                self.page.doc = Rc::new(doc);
                self.scroll_to_fragment();
                return;
            }
        }
        if !matches!(req.url.scheme(), "http" | "https" | "file") {
            self.message(format!("Can't open {} links (Y copies it)", req.url.scheme()));
            return;
        }
        self.cancel();
        self.generation += 1;
        let (generation, tx, client, r) = (self.generation, self.tx.clone(), self.client.clone(), req.clone());
        let task = tokio::spawn(async move {
            let result = net::fetch(&client, &r).await;
            let _ = tx.send(Msg::Loaded { generation, req: r, push, restore, result });
        });
        self.loading = Some(Loading { url: req.url.to_string(), since: Instant::now(), task: task.abort_handle() });
    }

    fn cancel(&mut self) {
        if let Some(l) = self.loading.take() {
            l.task.abort();
        }
        self.generation += 1;
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Loaded { generation, req, push, restore, result } => {
                if generation != self.generation {
                    return;
                }
                self.loading = None;
                match result {
                    Ok(Outcome::Doc(doc)) => self.show(req, Rc::new(doc), push, restore, true),
                    Ok(Outcome::Downloaded { path, bytes }) => {
                        self.message(format!("Saved {} ({})", path.display(), human_size(bytes)));
                    }
                    Err(e) => {
                        let text = pages::error(req.url.as_str(), &e);
                        let doc = Document { url: req.url.clone(), mime: "text/html".into(), text, status: 0, hidden: Vec::new() };
                        self.show(req, Rc::new(doc), push, None, false);
                    }
                }
            }
            Msg::Refresh { generation, url } => {
                if generation == self.generation && self.loading.is_none() {
                    self.load(Request::get(url), false, None);
                }
            }
        }
    }

    fn show(&mut self, req: Request, doc: Rc<Document>, push: bool, restore: Option<(usize, Option<usize>)>, cache: bool) {
        let cached = cache.then(|| doc.clone());
        if push || self.history.is_empty() {
            self.save_position();
            if !self.history.is_empty() {
                self.history.truncate(self.index + 1);
            }
            self.history.push(Entry { req: req.clone(), scroll: 0, focus: None, doc: cached });
            self.index = self.history.len() - 1;
        } else if let Some(e) = self.history.get_mut(self.index) {
            e.req = req.clone();
            e.doc = cached;
        }
        self.trim_cache();
        if doc.status >= 400 {
            self.message(format!("HTTP error {}", doc.status));
        }
        self.page = Page::new(req, doc, false, self.page.width);
        self.find = None;
        self.mode = Mode::Normal;
        match restore {
            Some((scroll, focus)) => {
                self.page.scroll = scroll;
                self.page.focus = focus.filter(|&f| f < self.page.layout.targets.len());
                self.clamp_scroll();
            }
            None => self.scroll_to_fragment(),
        }
        self.schedule_refresh();
    }

    fn schedule_refresh(&mut self) {
        let refresh = match &self.page.content {
            Content::Html(d) => d.refresh.clone(),
            Content::Text => None,
        };
        let Some((delay, url)) = refresh.filter(|(d, _)| *d <= 10) else {
            self.refreshes = 0;
            return;
        };
        if self.refreshes >= MAX_REFRESHES {
            self.message("Stopped following <meta refresh> redirects");
            return;
        }
        self.refreshes += 1;
        let (generation, tx) = (self.generation, self.tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let _ = tx.send(Msg::Refresh { generation, url });
        });
    }

    fn scroll_to_fragment(&mut self) {
        let anchors = &self.page.layout.anchors;
        let line = self.page.url().fragment().and_then(|f| {
            anchors.get(f).copied().or_else(|| {
                // Fragments are percent-encoded; ids usually aren't.
                let query = format!("x={}", f.replace('+', "%2B"));
                let decoded = url::form_urlencoded::parse(query.as_bytes()).next()?.1.into_owned();
                anchors.get(&decoded).copied()
            })
        });
        self.page.scroll = line.unwrap_or(0);
        self.clamp_scroll();
    }

    fn save_position(&mut self) {
        if let Some(e) = self.history.get_mut(self.index) {
            e.scroll = self.page.scroll;
            e.focus = self.page.focus;
        }
    }

    fn trim_cache(&mut self) {
        for (i, e) in self.history.iter_mut().enumerate() {
            if i.abs_diff(self.index) > CACHED_ENTRIES {
                e.doc = None;
            }
        }
    }

    fn go(&mut self, delta: isize) {
        let Some(new) = self.index.checked_add_signed(delta).filter(|&i| i < self.history.len()) else {
            self.message(if delta < 0 { "No previous page" } else { "No next page" });
            return;
        };
        self.save_position();
        self.index = new;
        let (req, scroll, focus, doc) = {
            let e = &self.history[new];
            (e.req.clone(), e.scroll, e.focus, e.doc.clone())
        };
        match doc {
            Some(doc) => {
                self.cancel();
                self.page = Page::new(req, doc, false, self.page.width);
                self.page.scroll = scroll;
                self.page.focus = focus.filter(|&f| f < self.page.layout.targets.len());
                self.find = None;
                self.clamp_scroll();
            }
            None => self.load(req, false, Some((scroll, focus))),
        }
    }

    fn reload(&mut self) {
        let restore = Some((self.page.scroll, self.page.focus));
        self.load(self.page.req.clone(), false, restore);
    }

    fn toggle_source(&mut self) {
        let source = !self.page.source;
        let (req, doc) = (self.page.req.clone(), self.page.doc.clone());
        self.page = Page::new(req, doc, source, self.page.width);
        if source {
            self.page.title = format!("Source of {}", self.page.url());
        }
        self.find = None;
    }

    /// Turns what the user typed into the URL bar into a request: a URL, a file, or a search.
    pub fn resolve_input(&self, input: &str) -> Request {
        resolve_input(input, &self.search)
    }

    // ---- scrolling and focus ----------------------------------------------

    fn max_scroll(&self) -> usize {
        self.page.layout.lines.len().saturating_sub(self.height.saturating_sub(1).max(1))
    }

    fn clamp_scroll(&mut self) {
        self.page.scroll = self.page.scroll.min(self.max_scroll());
    }

    fn scroll_by(&mut self, delta: isize) {
        self.page.scroll = self.page.scroll.saturating_add_signed(delta);
        self.clamp_scroll();
    }

    fn visible(&self, line: usize) -> bool {
        line >= self.page.scroll && line < self.page.scroll + self.height
    }

    fn reveal(&mut self, line: usize) {
        if line < self.page.scroll {
            self.page.scroll = line;
        } else if line >= self.page.scroll + self.height {
            self.page.scroll = line.saturating_sub(self.height / 3);
        }
        self.clamp_scroll();
    }

    fn focus_step(&mut self, forward: bool) {
        let lines = &self.page.layout.target_lines;
        let shown = |t: &usize| lines[*t] != usize::MAX;
        let next = match self.page.focus {
            Some(f) if f < lines.len() && self.visible(lines[f]) => {
                if forward { (f + 1..lines.len()).find(shown) } else { (0..f).rev().find(shown) }
            }
            _ => {
                let top = self.page.scroll;
                if forward {
                    (0..lines.len()).find(|&t| lines[t] != usize::MAX && lines[t] >= top)
                } else {
                    (0..lines.len()).rev().find(|&t| lines[t] != usize::MAX && lines[t] < top + self.height)
                }
            }
        };
        match next {
            Some(t) => {
                self.page.focus = Some(t);
                self.reveal(lines[t]);
            }
            None => self.message(if forward { "No more links below" } else { "No more links above" }),
        }
    }

    // ---- activation ---------------------------------------------------------

    fn activate(&mut self, target: usize) {
        self.page.focus = Some(target);
        match self.page.layout.targets.get(target).cloned() {
            Some(Target::Link(url)) => self.navigate(Request::get(url), true),
            Some(Target::Field(fi)) => self.activate_field(fi),
            None => {}
        }
    }

    fn activate_field(&mut self, fi: usize) {
        let Content::Html(dom) = &mut self.page.content else { return };
        let field = &mut dom.forms.fields[fi];
        match field.kind {
            Kind::Text | Kind::Password | Kind::TextArea => {
                self.mode = Mode::Edit { field: fi, editor: Editor::new(&field.value) };
            }
            Kind::Checkbox => {
                field.checked = !field.checked;
                self.page.refresh();
            }
            Kind::Radio => {
                dom.forms.check_radio(fi);
                self.page.refresh();
            }
            Kind::Select => {
                if !field.options.is_empty() {
                    self.mode = Mode::Select { field: fi, cursor: field.selected };
                }
            }
            Kind::Submit | Kind::Image => self.submit(fi, Some(fi)),
            Kind::Button => self.message("This button needs JavaScript, which Lineal doesn't run"),
            Kind::Reset => self.message("Reset buttons aren't supported yet"),
            Kind::File => self.message("File uploads aren't supported yet"),
            Kind::Hidden => {}
        }
    }

    fn submit(&mut self, field: usize, submitter: Option<usize>) {
        let Content::Html(dom) = &self.page.content else { return };
        let Some(form) = dom.forms.fields[field].form else {
            self.message("This field isn't part of a form");
            return;
        };
        let submitter = submitter.or_else(|| dom.forms.default_button(form));
        match dom.forms.submit(form, submitter, &dom.url, &dom.base) {
            Some(req) => self.navigate(req, true),
            None => self.message("This form needs JavaScript, which Lineal doesn't run"),
        }
    }

    fn set_field_value(&mut self, fi: usize, value: String) {
        if let Content::Html(dom) = &mut self.page.content {
            dom.forms.fields[fi].value = value;
            self.page.refresh();
        }
    }

    /// The target after `fi`'s target that is a text-like field, for Tab in field editing.
    fn next_text_field(&self, fi: usize) -> Option<(usize, usize)> {
        let Content::Html(dom) = &self.page.content else { return None };
        let targets = &self.page.layout.targets;
        let from = targets.iter().position(|t| *t == Target::Field(fi))?;
        targets.iter().enumerate().skip(from + 1).find_map(|(ti, t)| match t {
            Target::Field(f) if matches!(dom.forms.fields[*f].kind, Kind::Text | Kind::Password | Kind::TextArea) => {
                Some((ti, *f))
            }
            _ => None,
        })
    }

    pub fn field_name(&self, fi: usize) -> String {
        match &self.page.content {
            Content::Html(dom) => {
                let f = &dom.forms.fields[fi];
                if !f.placeholder.is_empty() { f.placeholder.clone() } else if !f.name.is_empty() { f.name.clone() } else { "text".into() }
            }
            Content::Text => String::new(),
        }
    }

    pub fn select_options(&self, fi: usize) -> Vec<String> {
        match &self.page.content {
            Content::Html(dom) => dom.forms.fields[fi].options.iter().map(|o| o.1.clone()).collect(),
            Content::Text => Vec::new(),
        }
    }

    /// What the status bar says about the focused target.
    pub fn focus_description(&self) -> Option<String> {
        let t = self.page.focus?;
        match self.page.layout.targets.get(t)? {
            Target::Link(u) => Some(u.to_string()),
            Target::Field(fi) => {
                let Content::Html(dom) = &self.page.content else { return None };
                let f = &dom.forms.fields[*fi];
                let what = match f.kind {
                    Kind::Text | Kind::TextArea => "text field (Enter to edit)",
                    Kind::Password => "password field (Enter to edit)",
                    Kind::Checkbox => "checkbox (Enter to toggle)",
                    Kind::Radio => "radio button (Enter to select)",
                    Kind::Select => "choice (Enter to pick)",
                    Kind::Submit | Kind::Image => "submit button",
                    _ => "button",
                };
                Some(if f.name.is_empty() { what.to_string() } else { format!("{what}: {}", f.name) })
            }
        }
    }

    // ---- find -----------------------------------------------------------------

    fn find(&mut self, query: &str) {
        if query.is_empty() {
            self.find = None;
            return;
        }
        self.find = Some(Find { query: query.to_string(), matches: Vec::new(), current: 0 });
        self.rerun_find();
        let Some(f) = &mut self.find else { return };
        if f.matches.is_empty() {
            let q = f.query.clone();
            self.find = None;
            self.message(format!("Not found: {q}"));
            return;
        }
        let top = self.page.scroll;
        f.current = f.matches.iter().position(|m| m.0 >= top).unwrap_or(0);
        let (line, n) = (f.matches[f.current].0, f.matches.len());
        self.reveal(line);
        self.message(format!("{n} match{}", if n == 1 { "" } else { "es" }));
    }

    fn rerun_find(&mut self) {
        let Some(f) = &mut self.find else { return };
        let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
        let q: Vec<char> = f.query.chars().map(lower).collect();
        f.matches.clear();
        for (li, line) in self.page.layout.lines.iter().enumerate() {
            let chars: Vec<char> = line.text().chars().collect();
            let lc: Vec<char> = chars.iter().map(|&c| lower(c)).collect();
            let mut i = 0;
            while i + q.len() <= lc.len() {
                if lc[i..i + q.len()] == q[..] {
                    let col: usize = chars[..i].iter().map(|c| c.width().unwrap_or(0)).sum();
                    let w: usize = chars[i..i + q.len()].iter().map(|c| c.width().unwrap_or(0)).sum();
                    f.matches.push((li, col, w));
                    i += q.len();
                } else {
                    i += 1;
                }
            }
        }
        f.current = f.current.min(f.matches.len().saturating_sub(1));
    }

    fn find_step(&mut self, forward: bool) {
        let Some(f) = &mut self.find else {
            self.message("No search; press / to find");
            return;
        };
        if f.matches.is_empty() {
            return;
        }
        let n = f.matches.len();
        f.current = if forward { (f.current + 1) % n } else { (f.current + n - 1) % n };
        let (line, cur) = (f.matches[f.current].0, f.current);
        self.reveal(line);
        self.message(format!("Match {} of {n}", cur + 1));
    }

    // ---- hints ----------------------------------------------------------------

    fn start_hints(&mut self) {
        let mut found: Vec<(usize, usize, usize)> = Vec::new();
        let end = (self.page.scroll + self.height).min(self.page.layout.lines.len());
        for li in self.page.scroll..end {
            let mut col = 0;
            for s in &self.page.layout.lines[li].spans {
                if let Some(t) = s.target
                    && !found.iter().any(|f| f.0 == t)
                {
                    found.push((t, li, col));
                }
                col += unicode_width::UnicodeWidthStr::width(s.text.as_str());
            }
        }
        if found.is_empty() {
            self.message("No links on screen");
            return;
        }
        let keys: Vec<char> = HINT_KEYS.chars().collect();
        let two = found.len() > keys.len();
        found.truncate(keys.len() * keys.len());
        let hints = found
            .into_iter()
            .enumerate()
            .map(|(i, (target, line, col))| {
                let label = if two { format!("{}{}", keys[i / keys.len()], keys[i % keys.len()]) } else { keys[i].to_string() };
                Hint { label, target, line, col }
            })
            .collect();
        self.mode = Mode::Hints { hints, typed: String::new() };
    }

    // ---- input ----------------------------------------------------------------

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(text) => match &mut self.mode {
                Mode::Url(e) | Mode::Find(e) | Mode::Edit { editor: e, .. } => e.paste(&text),
                _ => {}
            },
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::Down(MouseButton::Left) if matches!(self.mode, Mode::Normal) => {
                let (x0, y0) = self.origin;
                if m.row < y0 || m.column < x0 {
                    return;
                }
                let line = self.page.scroll + (m.row - y0) as usize;
                let col = (m.column - x0) as usize;
                if (m.row - y0) as usize >= self.height {
                    return;
                }
                let Some(l) = self.page.layout.lines.get(line) else { return };
                let mut x = 0;
                for s in &l.spans {
                    let w = unicode_width::UnicodeWidthStr::width(s.text.as_str());
                    if col >= x && col < x + w {
                        if let Some(t) = s.target {
                            self.activate(t);
                        }
                        return;
                    }
                    x += w;
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Normal => self.normal_key(key),
            Mode::Url(mut e) => match e.key(key) {
                Edit::Submit => {
                    let req = self.resolve_input(&e.text());
                    self.navigate(req, true);
                }
                Edit::Cancel => {}
                _ => self.mode = Mode::Url(e),
            },
            Mode::Find(mut e) => match e.key(key) {
                Edit::Submit => self.find(&e.text()),
                Edit::Cancel => {}
                _ => self.mode = Mode::Find(e),
            },
            Mode::Edit { field, mut editor } => match editor.key(key) {
                Edit::Submit => {
                    self.set_field_value(field, editor.text());
                    self.submit(field, None);
                }
                Edit::Next => {
                    self.set_field_value(field, editor.text());
                    if let Some((ti, next)) = self.next_text_field(field) {
                        self.page.focus = Some(ti);
                        let line = self.page.layout.target_lines[ti];
                        self.reveal(line);
                        let value = match &self.page.content {
                            Content::Html(d) => d.forms.fields[next].value.clone(),
                            Content::Text => String::new(),
                        };
                        self.mode = Mode::Edit { field: next, editor: Editor::new(&value) };
                    }
                }
                Edit::Cancel => {}
                Edit::Continue => self.mode = Mode::Edit { field, editor },
            },
            Mode::Select { field, cursor } => {
                let n = self.select_options(field).len();
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                        self.mode = Mode::Select { field, cursor: (cursor + 1).min(n.saturating_sub(1)) }
                    }
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                        self.mode = Mode::Select { field, cursor: cursor.saturating_sub(1) }
                    }
                    KeyCode::Home => self.mode = Mode::Select { field, cursor: 0 },
                    KeyCode::End => self.mode = Mode::Select { field, cursor: n.saturating_sub(1) },
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        if let Content::Html(dom) = &mut self.page.content {
                            dom.forms.fields[field].selected = cursor;
                            self.page.refresh();
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    _ => self.mode = Mode::Select { field, cursor },
                }
            }
            Mode::Hints { hints, mut typed } => match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    typed.push(c);
                    if let Some(h) = hints.iter().find(|h| h.label == typed) {
                        let t = h.target;
                        self.activate(t);
                    } else if hints.iter().any(|h| h.label.starts_with(&typed)) {
                        self.mode = Mode::Hints { hints, typed };
                    }
                }
                KeyCode::Backspace => {
                    typed.pop();
                    self.mode = Mode::Hints { hints, typed };
                }
                _ => {}
            },
        }
    }

    fn normal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.height.saturating_sub(2).max(1) as isize;
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('d') if ctrl => self.scroll_by(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_by(-page / 2),
            KeyCode::Char('f') if ctrl => self.scroll_by(page),
            KeyCode::Char('b') if ctrl => self.scroll_by(-page),
            KeyCode::Char('l') if ctrl => self.mode = Mode::Url(Editor::default()),
            KeyCode::Left if alt => self.go(-1),
            KeyCode::Right if alt => self.go(1),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll_by(page),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::Char('g') | KeyCode::Home => self.page.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.page.scroll = self.max_scroll(),
            KeyCode::Tab => self.focus_step(true),
            KeyCode::BackTab => self.focus_step(false),
            KeyCode::Enter => match self.page.focus {
                Some(t) => self.activate(t),
                None => self.focus_step(true),
            },
            KeyCode::Char('o') => self.mode = Mode::Url(Editor::default()),
            KeyCode::Char('O') => {
                let url = if self.page.url().scheme() == "about" { String::new() } else { self.page.url().to_string() };
                self.mode = Mode::Url(Editor::new(&url));
            }
            KeyCode::Char('f') => self.start_hints(),
            KeyCode::Char('/') => self.mode = Mode::Find(Editor::default()),
            KeyCode::Char('n') => self.find_step(true),
            KeyCode::Char('N') => self.find_step(false),
            KeyCode::Char('H') | KeyCode::Backspace => self.go(-1),
            KeyCode::Char('L') => self.go(1),
            KeyCode::Char('r') | KeyCode::F(5) => self.reload(),
            KeyCode::Char('\\') => self.toggle_source(),
            KeyCode::Char('?') | KeyCode::F(1) => self.navigate(Request::get(Url::parse("about:help").unwrap()), true),
            KeyCode::Char('y') => {
                let url = self.page.url().to_string();
                copy(&url);
                self.message(format!("Copied {url}"));
            }
            KeyCode::Char('Y') => match self.page.focus.and_then(|t| self.page.layout.targets.get(t)) {
                Some(Target::Link(u)) => {
                    let u = u.to_string();
                    copy(&u);
                    self.message(format!("Copied {u}"));
                }
                _ => self.message("No link focused"),
            },
            KeyCode::Esc => {
                if self.loading.is_some() {
                    self.cancel();
                    self.message("Stopped");
                } else {
                    self.page.focus = None;
                    self.find = None;
                    self.message = None;
                }
            }
            _ => {}
        }
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
    let name = host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
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
        Some(rest) => std::path::PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?).join(rest),
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

/// Copies to the system clipboard with an OSC 52 escape; works over SSH in most modern terminals.
fn copy(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
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
        assert_eq!(r("v1.2"), "https://lite.duckduckgo.com/lite/?q=v1.2");
        assert_eq!(r(""), "about:home");
    }

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }
}
