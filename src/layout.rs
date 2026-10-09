//! Turns a DOM into styled, wrapped terminal lines.
//!
//! This is a small text-mode layout engine: block/inline flow, word wrapping, lists, quotes,
//! preformatted text, column-based tables, links and form fields. It knows nothing about the
//! terminal UI; `ui.rs` maps the style bits below to colors.

use std::collections::{HashMap, HashSet};

use ego_tree::{NodeId, NodeRef};
use scraper::node::Element;
use scraper::{Html, Node};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use url::Url;

use crate::forms::{Forms, Kind};

/// Style bits of a span.
pub mod style {
    pub const BOLD: u16 = 1;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const STRIKE: u16 = 1 << 3;
    pub const CODE: u16 = 1 << 4;
    pub const MARK: u16 = 1 << 5;
    pub const DIM: u16 = 1 << 6;
    pub const H1: u16 = 1 << 7;
    pub const H2: u16 = 1 << 8;
    pub const H3: u16 = 1 << 9;
    pub const LINK: u16 = 1 << 10;
    pub const FIELD: u16 = 1 << 11;
    pub const IMAGE: u16 = 1 << 12;
}
use style::*;

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: u16,
    pub target: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
    fn is_blank(&self) -> bool {
        self.spans.iter().all(|s| s.text.trim().is_empty())
    }
}

/// Something the user can activate.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Link(Url),
    /// Index into `Forms::fields`.
    Field(usize),
}

#[derive(Debug, Default)]
pub struct Layout {
    pub lines: Vec<Line>,
    pub targets: Vec<Target>,
    /// First line each target appears on.
    pub target_lines: Vec<usize>,
    /// Element ids (fragment targets) -> line.
    pub anchors: HashMap<String, usize>,
}

impl Layout {
    pub fn plain_text(&self) -> String {
        let mut s = String::new();
        for l in &self.lines {
            s += l.text().trim_end();
            s.push('\n');
        }
        s
    }

    /// Lays out plain text, hard-wrapping long lines.
    pub fn text(text: &str, width: usize) -> Layout {
        let forms = Forms::default();
        let hidden = HashSet::new();
        let base = Url::parse("about:blank").unwrap();
        let ctx = Ctx { forms: &forms, hidden: &hidden, base: &base, number_links: false };
        let mut b = Builder::new(&ctx, width.max(10));
        b.pre = 1;
        b.pre_text(text);
        b.into_layout()
    }
}

pub struct Ctx<'a> {
    pub forms: &'a Forms,
    pub hidden: &'a HashSet<NodeId>,
    pub base: &'a Url,
    /// Append `[n]` after each link (used by `--dump`).
    pub number_links: bool,
}

pub fn layout(html: &Html, ctx: &Ctx, width: usize) -> Layout {
    let mut b = Builder::new(ctx, width.max(10));
    b.children(html.tree.root());
    b.into_layout()
}

const SKIPPED: &[&str] = &[
    "head", "script", "style", "template", "svg", "math", "canvas", "datalist", "map", "area", "object", "embed",
    "param", "source", "track", "meta", "link", "base", "title", "noembed", "noframes",
];

const BLOCKS: &[&str] = &[
    "html", "body", "div", "section", "article", "main", "nav", "header", "footer", "aside", "center", "hgroup",
    "search", "caption", "figcaption", "legend", "form", "fieldset", "details", "dialog", "tr", "td", "th", "tbody",
    "thead", "tfoot", "option", "optgroup", "frameset", "noscript", "address", "picture",
];

/// Blocks with a blank line before and after.
const SPACED: &[&str] = &["p", "figure", "h1", "h2", "h3", "h4", "h5", "h6", "pre", "listing", "xmp", "plaintext", "blockquote"];

const BULLETS: &[&str] = &["• ", "◦ ", "▪ "];

fn is_hidden(e: &Element) -> bool {
    if e.attr("hidden").is_some() || e.attr("aria-hidden").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return true;
    }
    if e.name() == "dialog" && e.attr("open").is_none() {
        return true;
    }
    if e.name() == "input" && e.attr("type").is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
        return true;
    }
    match e.attr("style") {
        Some(s) => {
            let s: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
            s.split(';').any(|d| d.starts_with("display:none") || d.starts_with("visibility:hidden"))
        }
        None => false,
    }
}

struct Indent {
    first: String,
    rest: String,
    style: u16,
    used: bool,
}

struct List {
    ordered: bool,
    next: i64,
}

/// One table cell, rendered.
struct Cell {
    width: usize,
    lines: Vec<Line>,
    anchors: Vec<(String, usize)>,
}

struct Builder<'a> {
    ctx: &'a Ctx<'a>,
    width: usize,
    lines: Vec<Line>,
    cur: Vec<Span>,
    cur_w: usize,
    /// The current line has been started (prefix emitted).
    started: bool,
    /// The current line has content beyond its prefix.
    content: bool,
    indents: Vec<Indent>,
    lists: Vec<List>,
    style: u16,
    target: Option<usize>,
    pre: usize,
    /// Whitespace is pending between inline content.
    space: bool,
    /// A blank line is pending before the next content.
    blank: bool,
    targets: Vec<Target>,
    anchors: HashMap<String, usize>,
    pending_anchors: Vec<String>,
    /// Counts emitted content, to detect empty links.
    emitted: usize,
    table_depth: usize,
}

impl<'a> Builder<'a> {
    fn new(ctx: &'a Ctx<'a>, width: usize) -> Self {
        Builder {
            ctx,
            width,
            lines: Vec::new(),
            cur: Vec::new(),
            cur_w: 0,
            started: false,
            content: false,
            indents: Vec::new(),
            lists: Vec::new(),
            style: 0,
            target: None,
            pre: 0,
            space: false,
            blank: false,
            targets: Vec::new(),
            anchors: HashMap::new(),
            pending_anchors: Vec::new(),
            emitted: 0,
            table_depth: 0,
        }
    }

    fn into_layout(mut self) -> Layout {
        self.finish_line();
        let last = self.lines.len().saturating_sub(1);
        for a in self.pending_anchors.drain(..) {
            self.anchors.entry(a).or_insert(last);
        }
        while self.lines.last().is_some_and(Line::is_blank) {
            self.lines.pop();
        }
        let mut target_lines = vec![usize::MAX; self.targets.len()];
        for (i, line) in self.lines.iter().enumerate() {
            for s in &line.spans {
                if let Some(t) = s.target {
                    target_lines[t] = target_lines[t].min(i);
                }
            }
        }
        Layout { lines: self.lines, targets: self.targets, target_lines, anchors: self.anchors }
    }

    // ---- line management -------------------------------------------------

    fn prefix_width(&self) -> usize {
        let w: usize = self.indents.iter().map(|i| if i.used { i.rest.width() } else { i.first.width() }).sum();
        w.min(self.width / 2)
    }

    fn start_line(&mut self) {
        if self.started {
            return;
        }
        if self.blank {
            if self.lines.last().is_some_and(|l| !l.is_blank()) {
                self.lines.push(Line::default());
            }
            self.blank = false;
        }
        let at = self.lines.len();
        for a in self.pending_anchors.drain(..) {
            self.anchors.entry(a).or_insert(at);
        }
        let mut w = 0;
        let limit = self.width / 2;
        for ind in &mut self.indents {
            let s = if ind.used { ind.rest.clone() } else { ind.first.clone() };
            ind.used = true;
            if w + s.width() > limit {
                continue;
            }
            w += s.width();
            if !s.is_empty() {
                push_span(&mut self.cur, s, ind.style, None);
            }
        }
        self.cur_w = w;
        self.started = true;
        self.content = false;
    }

    fn finish_line(&mut self) {
        if self.started {
            while let Some(last) = self.cur.last_mut() {
                let trimmed = last.text.trim_end_matches(' ').len();
                if trimmed == 0 && last.target.is_none() {
                    self.cur.pop();
                } else {
                    last.text.truncate(trimmed);
                    break;
                }
            }
            self.lines.push(Line { spans: std::mem::take(&mut self.cur) });
            self.started = false;
            self.cur_w = 0;
            self.content = false;
        }
        self.space = false;
    }

    fn block(&mut self, spaced: bool) {
        self.finish_line();
        if spaced {
            self.blank = true;
        }
    }

    fn line_break(&mut self) {
        if !self.started {
            self.start_line();
        }
        self.finish_line();
    }

    fn push(&mut self, text: &str, style: u16, target: Option<usize>) {
        if text.is_empty() {
            return;
        }
        self.cur_w += text.width();
        self.content = true;
        self.emitted += 1;
        push_span(&mut self.cur, text.to_string(), style, target);
    }

    /// Adds an unbreakable run of text (a word, or a form field), wrapping before it if needed.
    fn word(&mut self, word: &str) {
        let w = word.width();
        let space = self.space && self.content;
        if self.started && self.content && self.cur_w + space as usize + w > self.width {
            self.finish_line();
        }
        self.start_line();
        if self.space && self.content {
            // Inside one link the space stays part of it (underlined); between links it's plain.
            match self.cur.last() {
                Some(s) if s.target == self.target => {
                    let (style, target) = (s.style, s.target);
                    self.push(" ", style, target)
                }
                _ => self.push(" ", 0, None),
            }
        }
        self.space = false;
        if self.cur_w + w <= self.width {
            self.push(word, self.style, self.target);
            return;
        }
        // Longer than a whole line: hard-break it.
        let mut chunk = String::new();
        for c in word.chars() {
            let cw = c.width().unwrap_or(0);
            if self.cur_w + chunk.width() + cw > self.width && (self.content || !chunk.is_empty()) {
                let piece = std::mem::take(&mut chunk);
                self.push(&piece, self.style, self.target);
                self.finish_line();
                self.start_line();
            }
            chunk.push(c);
        }
        self.push(&chunk, self.style, self.target);
    }

    fn text(&mut self, text: &str) {
        if self.pre > 0 {
            return self.pre_text(text);
        }
        let mut word = String::new();
        for c in text.chars() {
            match c {
                '\u{a0}' => word.push(' '),
                '\u{ad}' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' => {}
                c if c.is_whitespace() => {
                    if !word.is_empty() {
                        self.word(&word);
                        word.clear();
                    }
                    self.space = true;
                }
                c if c.is_control() => {}
                c => word.push(c),
            }
        }
        if !word.is_empty() {
            self.word(&word);
        }
    }

    fn pre_text(&mut self, text: &str) {
        for (i, segment) in text.split('\n').enumerate() {
            if i > 0 {
                self.line_break();
            }
            if segment.is_empty() {
                continue;
            }
            self.start_line();
            let start = self.prefix_width();
            let mut run = String::new();
            for c in segment.chars() {
                let (s, cw) = match c {
                    '\t' => {
                        let col = self.cur_w + run.width() - start;
                        let n = 8 - col % 8;
                        (" ".repeat(n), n)
                    }
                    '\r' => continue,
                    c if c.is_control() => continue,
                    c => (c.to_string(), c.width().unwrap_or(0)),
                };
                if self.cur_w + run.width() + cw > self.width && (self.content || !run.is_empty()) {
                    let piece = std::mem::take(&mut run);
                    self.push(&piece, self.style, self.target);
                    self.finish_line();
                    self.start_line();
                }
                run.push_str(&s);
            }
            self.push(&run, self.style, self.target);
        }
    }

    // ---- tree walking ----------------------------------------------------

    fn children(&mut self, node: NodeRef<Node>) {
        for child in node.children() {
            self.node(child);
        }
    }

    fn node(&mut self, node: NodeRef<Node>) {
        match node.value() {
            Node::Text(t) => self.text(t),
            Node::Element(e) => self.element(node, e),
            Node::Document | Node::Fragment => self.children(node),
            _ => {}
        }
    }

    fn skip(&self, node: NodeRef<Node>, e: &Element) -> bool {
        SKIPPED.contains(&e.name()) || is_hidden(e) || self.ctx.hidden.contains(&node.id())
    }

    fn with_style(&mut self, node: NodeRef<Node>, add: u16) {
        let saved = self.style;
        self.style |= add;
        self.children(node);
        self.style = saved;
    }

    fn element(&mut self, node: NodeRef<Node>, e: &Element) {
        if self.skip(node, e) {
            return;
        }
        if let Some(id) = e.id() {
            self.pending_anchors.push(id.to_string());
        }
        if e.name() == "a"
            && let Some(name) = e.attr("name")
        {
            self.pending_anchors.push(name.to_string());
        }
        let name = e.name();
        match name {
            "br" => self.line_break(),
            "hr" => {
                self.block(false);
                self.start_line();
                let n = self.width.saturating_sub(self.cur_w);
                self.push(&"─".repeat(n), DIM, None);
                self.block(false);
            }
            "img" => self.image(e),
            "a" => self.link(node, e),
            "input" | "select" | "textarea" | "button" => self.field(node),
            "iframe" | "frame" => {
                if let Some(src) = e.attr("src").and_then(|s| self.ctx.base.join(s).ok()) {
                    let label = e.attr("title").filter(|t| !t.trim().is_empty()).unwrap_or("frame");
                    self.block(false);
                    self.atom_link(&format!("[{} {}]", "frame:", label.trim()), src);
                    self.block(false);
                }
            }
            "video" | "audio" => {
                let src = e.attr("src").map(str::to_string).or_else(|| {
                    node.children().find_map(|c| {
                        let ce = c.value().as_element()?;
                        (ce.name() == "source").then(|| ce.attr("src").map(str::to_string)).flatten()
                    })
                });
                if let Some(url) = src.and_then(|s| self.ctx.base.join(&s).ok()) {
                    self.atom_link(&format!("[{name}]"), url);
                }
            }
            "ul" | "ol" | "menu" | "dir" => {
                let nested = !self.lists.is_empty();
                self.block(!nested);
                let start = e.attr("start").and_then(|s| s.trim().parse().ok()).unwrap_or(1);
                self.lists.push(List { ordered: name == "ol", next: start });
                self.children(node);
                self.lists.pop();
                self.block(!nested);
            }
            "li" => self.list_item(node, e),
            "dl" => {
                self.block(self.lists.is_empty());
                self.children(node);
                self.block(self.lists.is_empty());
            }
            "dt" => {
                self.block(false);
                self.with_style(node, BOLD);
                self.block(false);
            }
            "dd" => self.indented(node, "    ", "    ", 0),
            "blockquote" => {
                self.block(true);
                self.indented(node, "│ ", "│ ", DIM);
                self.block(true);
            }
            "pre" | "listing" | "xmp" | "plaintext" => {
                self.block(true);
                self.pre += 1;
                self.with_style(node, CODE);
                self.pre -= 1;
                self.block(true);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = match name {
                    "h1" => H1,
                    "h2" => H2,
                    _ => H3,
                };
                self.block(true);
                self.with_style(node, BOLD | level);
                self.block(true);
            }
            "table" => self.table(node, e),
            "summary" => {
                self.block(false);
                self.word("▸");
                self.space = true;
                self.with_style(node, BOLD);
                self.block(false);
            }
            "q" => {
                self.text("“");
                self.children(node);
                self.text("”");
            }
            "sup" => {
                self.text("^");
                self.children(node);
            }
            "rt" => {
                self.text("(");
                self.children(node);
                self.text(")");
            }
            "b" | "strong" => self.with_style(node, BOLD),
            "i" | "em" | "cite" | "dfn" | "var" => self.with_style(node, ITALIC),
            "u" | "ins" => self.with_style(node, UNDERLINE),
            "s" | "strike" | "del" => self.with_style(node, STRIKE),
            "code" | "kbd" | "samp" | "tt" => self.with_style(node, CODE),
            "mark" => self.with_style(node, MARK),
            _ if SPACED.contains(&name) => {
                self.block(true);
                self.children(node);
                self.block(true);
            }
            _ if BLOCKS.contains(&name) => {
                self.block(false);
                self.children(node);
                self.block(false);
            }
            _ => self.children(node),
        }
    }

    fn indented(&mut self, node: NodeRef<Node>, first: &str, rest: &str, style: u16) {
        self.block(false);
        self.indents.push(Indent { first: first.into(), rest: rest.into(), style, used: false });
        self.children(node);
        self.finish_line();
        self.indents.pop();
    }

    fn list_item(&mut self, node: NodeRef<Node>, e: &Element) {
        let depth = self.lists.len().saturating_sub(1);
        let marker = match self.lists.last_mut() {
            Some(l) if l.ordered => {
                if let Some(v) = e.attr("value").and_then(|v| v.trim().parse().ok()) {
                    l.next = v;
                }
                l.next += 1;
                format!("{}. ", l.next - 1)
            }
            _ => BULLETS[depth % BULLETS.len()].to_string(),
        };
        let rest = " ".repeat(marker.width());
        self.indented(node, &marker, &rest, 0);
    }

    fn add_target(&mut self, t: Target) -> usize {
        self.targets.push(t);
        self.targets.len() - 1
    }

    fn link_number(&mut self, idx: usize) {
        if self.ctx.number_links {
            let saved = (self.style, self.target);
            (self.style, self.target) = (DIM, None);
            self.word(&format!("[{}]", idx + 1));
            (self.style, self.target) = saved;
        }
    }

    fn atom_link(&mut self, label: &str, url: Url) {
        let idx = self.add_target(Target::Link(url));
        let saved = (self.style, self.target);
        (self.style, self.target) = (saved.0 | LINK, Some(idx));
        self.text(label);
        (self.style, self.target) = saved;
        self.link_number(idx);
    }

    fn link(&mut self, node: NodeRef<Node>, e: &Element) {
        let href = e.attr("href").map(str::trim).unwrap_or("");
        let url = if href.is_empty() || href.to_ascii_lowercase().starts_with("javascript:") {
            None
        } else {
            self.ctx.base.join(href).ok()
        };
        let Some(url) = url else { return self.children(node) };
        let idx = self.add_target(Target::Link(url.clone()));
        let saved = (self.style, self.target);
        (self.style, self.target) = (saved.0 | LINK, Some(idx));
        let before = self.emitted;
        self.children(node);
        if self.emitted == before {
            // Icon-only link: fall back to its accessible name or a piece of the URL.
            let label = e
                .attr("aria-label")
                .or(e.attr("title"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    url.path_segments()
                        .and_then(|s| s.filter(|s| !s.is_empty()).next_back().map(str::to_string))
                        .or(url.host_str().map(str::to_string))
                        .unwrap_or_else(|| "link".into())
                });
            self.text(&format!("[{label}]"));
        }
        (self.style, self.target) = saved;
        self.link_number(idx);
    }

    fn image(&mut self, e: &Element) {
        let alt: String = e.attr("alt").unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        if alt.is_empty() {
            return;
        }
        let alt: String = if alt.chars().count() > 60 { alt.chars().take(59).chain(['…']).collect() } else { alt };
        let saved = self.style;
        self.style |= IMAGE;
        self.text(&format!("[{alt}]"));
        self.style = saved;
    }

    fn field(&mut self, node: NodeRef<Node>) {
        let Some(&fi) = self.ctx.forms.by_node.get(&node.id()) else { return };
        if self.ctx.forms.fields[fi].kind == Kind::Hidden {
            return;
        }
        let label = self.ctx.forms.display(fi);
        let idx = self.add_target(Target::Field(fi));
        let saved = (self.style, self.target);
        (self.style, self.target) = (FIELD, Some(idx));
        self.word(&label);
        (self.style, self.target) = saved;
    }

    // ---- tables -----------------------------------------------------------

    fn table(&mut self, node: NodeRef<Node>, e: &Element) {
        let spaced = self.table_depth == 0;
        self.block(spaced);
        let mut caption = None;
        let mut rows: Vec<Vec<(NodeRef<Node>, usize)>> = Vec::new();
        let mut has_th = false;
        for child in node.children() {
            let Some(ce) = child.value().as_element() else { continue };
            if self.skip(child, ce) {
                continue;
            }
            match ce.name() {
                "caption" => caption = Some(child),
                "tr" => rows.push(self.table_row(child, &mut has_th)),
                "thead" | "tbody" | "tfoot" => {
                    for tr in child.children() {
                        if let Some(te) = tr.value().as_element()
                            && te.name() == "tr"
                            && !self.skip(tr, te)
                        {
                            rows.push(self.table_row(tr, &mut has_th));
                        }
                    }
                }
                _ => {}
            }
        }
        rows.retain(|r| !r.is_empty());
        if let Some(c) = caption {
            self.block(false);
            self.with_style(c, BOLD);
            self.block(false);
        }
        let ncols = rows.iter().map(|r| r.iter().map(|c| c.1).sum::<usize>()).max().unwrap_or(0);
        if ncols <= 1 || self.table_depth >= 6 || !self.table_grid(&rows, ncols, has_th || bordered(e)) {
            // One column (or no room for a grid): show cells one after another.
            for row in &rows {
                for (cell, _) in row {
                    self.block(false);
                    self.children(*cell);
                    self.block(false);
                }
            }
        }
        self.block(spaced);
    }

    fn table_row<'n>(&self, tr: NodeRef<'n, Node>, has_th: &mut bool) -> Vec<(NodeRef<'n, Node>, usize)> {
        let mut cells = Vec::new();
        for c in tr.children() {
            let Some(ce) = c.value().as_element() else { continue };
            if !matches!(ce.name(), "td" | "th") || self.skip(c, ce) {
                continue;
            }
            *has_th |= ce.name() == "th";
            let span = ce.attr("colspan").and_then(|s| s.trim().parse().ok()).unwrap_or(1usize).clamp(1, 50);
            cells.push((c, span));
        }
        cells
    }

    /// Lays the table out as a grid. Returns false (having emitted nothing) if it doesn't fit.
    fn table_grid(&mut self, rows: &[Vec<(NodeRef<Node>, usize)>], ncols: usize, bordered: bool) -> bool {
        let mut colmin = vec![0usize; ncols];
        let mut colmax = vec![0usize; ncols];
        for row in rows {
            let mut c = 0;
            for &(cell, span) in row {
                let (min, max) = self.measure(cell);
                if span == 1 && c < ncols {
                    colmin[c] = colmin[c].max(min.min(30));
                    colmax[c] = colmax[c].max(max);
                } else if max > 0 && c < ncols && colmax[c..(c + span).min(ncols)].iter().all(|&m| m == 0) {
                    // Only spanning cells in these columns: give the first column some width.
                    colmin[c] = colmin[c].max(min.min(30));
                    colmax[c] = colmax[c].max(max);
                }
                c += span;
            }
        }
        let active: Vec<bool> = colmax.iter().map(|&m| m > 0).collect();
        let k = active.iter().filter(|&&a| a).count();
        if k == 0 {
            return true;
        }
        let sep = if bordered { " │ " } else { "  " };
        let avail = (self.width - self.prefix_width()).saturating_sub(sep.width() * (k - 1));
        let min_sum: usize = colmin.iter().sum();
        if min_sum > avail {
            return false;
        }
        let max_sum: usize = colmax.iter().sum();
        let widths: Vec<usize> = if max_sum <= avail {
            colmax.clone()
        } else {
            let extra = avail - min_sum;
            let flex: usize = colmax.iter().zip(&colmin).map(|(a, b)| a - b.min(a)).sum();
            let mut w: Vec<usize> = colmin
                .iter()
                .zip(&colmax)
                .map(|(&mn, &mx)| mn + (mx.saturating_sub(mn) * extra).checked_div(flex).unwrap_or(0))
                .collect();
            // Hand out rounding leftovers.
            let mut left = avail - w.iter().sum::<usize>();
            for (i, wi) in w.iter_mut().enumerate() {
                if left == 0 {
                    break;
                }
                if active[i] && *wi < colmax[i] {
                    *wi += 1;
                    left -= 1;
                }
            }
            w
        };

        self.finish_line();
        for row in rows {
            let mut cells = Vec::new();
            let mut c = 0;
            for &(cell, span) in row {
                let range = c..(c + span).min(ncols);
                c += span;
                let n_active = range.clone().filter(|&i| active[i]).count();
                if n_active == 0 {
                    continue;
                }
                let w: usize = range.clone().map(|i| widths[i]).sum::<usize>() + sep.width() * (n_active - 1);
                let is_th = cell.value().as_element().is_some_and(|e| e.name() == "th");
                cells.push(self.render_cell(cell, w.max(1), if is_th { BOLD } else { 0 }));
            }
            // Pad short rows so separators line up.
            let used: usize = cells.iter().map(|c| c.width).sum::<usize>() + sep.width() * cells.len().saturating_sub(1);
            let total: usize = widths.iter().sum::<usize>() + sep.width() * (k - 1);
            if used < total && !cells.is_empty() {
                cells.last_mut().unwrap().width += total - used;
            }
            let height = cells.iter().map(|c| c.lines.len()).max().unwrap_or(0);
            if height == 0 {
                continue;
            }
            for i in 0..height {
                self.start_line();
                if i == 0 {
                    let at = self.lines.len();
                    for cell in &cells {
                        for (id, l) in &cell.anchors {
                            self.anchors.entry(id.clone()).or_insert(at + l);
                        }
                    }
                }
                for (ci, cell) in cells.iter().enumerate() {
                    let mut w = 0;
                    if let Some(line) = cell.lines.get(i) {
                        for s in &line.spans {
                            w += s.text.width();
                            self.push(&s.text, s.style, s.target);
                        }
                    }
                    if ci + 1 < cells.len() {
                        self.push(&" ".repeat(cell.width.saturating_sub(w)), 0, None);
                        self.push(sep, DIM, None);
                    }
                }
                self.finish_line();
            }
        }
        true
    }

    fn render_cell(&mut self, cell: NodeRef<Node>, width: usize, style: u16) -> Cell {
        let mut sub = Builder::new(self.ctx, width);
        sub.targets = std::mem::take(&mut self.targets);
        sub.style = self.style | style;
        sub.target = self.target;
        sub.table_depth = self.table_depth + 1;
        if let Some(id) = cell.value().as_element().and_then(|e| e.id()) {
            sub.pending_anchors.push(id.to_string());
        }
        sub.children(cell);
        sub.finish_line();
        let last = sub.lines.len().saturating_sub(1);
        for a in sub.pending_anchors.drain(..) {
            sub.anchors.entry(a).or_insert(last);
        }
        self.targets = std::mem::take(&mut sub.targets);
        let mut lines = sub.lines;
        let lead = lines.iter().take_while(|l| l.is_blank()).count();
        lines.drain(..lead);
        while lines.last().is_some_and(Line::is_blank) {
            lines.pop();
        }
        let anchors = sub.anchors.into_iter().map(|(k, v)| (k, v.saturating_sub(lead).min(lines.len().saturating_sub(1)))).collect();
        Cell { width, lines, anchors }
    }

    /// (min, max) content width of a subtree: longest word, and longest unwrapped line.
    fn measure(&self, node: NodeRef<Node>) -> (usize, usize) {
        let mut m = (0, 0, 0usize); // (min, max, current line)
        self.measure_into(node, &mut m);
        (m.0, m.1.max(m.2))
    }

    fn measure_into(&self, node: NodeRef<Node>, m: &mut (usize, usize, usize)) {
        for child in node.children() {
            match child.value() {
                Node::Text(t) => {
                    let mut first = true;
                    for word in t.split(|c: char| c.is_whitespace() && c != '\u{a0}') {
                        if !first && m.2 > 0 {
                            m.2 += 1;
                        }
                        first = false;
                        let w = word.width();
                        m.0 = m.0.max(w);
                        m.2 += w;
                    }
                }
                Node::Element(e) => {
                    if self.skip(child, e) {
                        continue;
                    }
                    let name = e.name();
                    let block = BLOCKS.contains(&name) && !matches!(name, "td" | "th")
                        || SPACED.contains(&name)
                        || matches!(name, "br" | "li" | "ul" | "ol" | "table" | "hr" | "dt" | "dd");
                    if block {
                        m.1 = m.1.max(m.2);
                        m.2 = 0;
                    }
                    match name {
                        "img" => {
                            let w = e.attr("alt").map(|a| a.trim().width().min(60) + 2).unwrap_or(0);
                            if w > 2 {
                                m.0 = m.0.max(w.min(12));
                                m.2 += w;
                            }
                        }
                        "input" | "select" | "textarea" | "button" => {
                            if let Some(&fi) = self.ctx.forms.by_node.get(&child.id()) {
                                let w = self.ctx.forms.display(fi).width();
                                m.0 = m.0.max(w);
                                m.2 += w + 1;
                            }
                        }
                        "td" | "th" => {
                            m.2 += 2;
                            self.measure_into(child, m);
                        }
                        "li" => {
                            m.2 += 2;
                            self.measure_into(child, m);
                        }
                        _ => self.measure_into(child, m),
                    }
                    if block {
                        m.1 = m.1.max(m.2);
                        m.2 = 0;
                    }
                }
                _ => {}
            }
        }
    }
}

fn bordered(e: &Element) -> bool {
    e.attr("border").is_some_and(|b| b.trim() != "0")
        || e.classes().any(|c| c.contains("wikitable") || c == "table" || c.contains("grid"))
}

fn push_span(spans: &mut Vec<Span>, text: String, style: u16, target: Option<usize>) {
    if let Some(last) = spans.last_mut()
        && last.style == style
        && last.target == target
    {
        last.text.push_str(&text);
        return;
    }
    spans.push(Span { text, style, target });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::Dom;

    fn render(html: &str, width: usize) -> (String, Layout) {
        let dom = Dom::parse(html, &Url::parse("https://example.org/").unwrap());
        let l = dom.layout(width, false);
        (l.plain_text(), l)
    }

    #[test]
    fn wraps_paragraphs() {
        let (t, _) = render("<p>one two three four five six</p><p>next</p>", 10);
        assert_eq!(t, "one two\nthree four\nfive six\n\nnext\n");
    }

    #[test]
    fn inline_styles_and_links() {
        let (t, l) = render(r#"<p>see <a href="/x">this <b>link</b></a>, ok</p>"#, 80);
        assert_eq!(t, "see this link, ok\n");
        assert_eq!(l.targets, vec![Target::Link(Url::parse("https://example.org/x").unwrap())]);
        let spans = &l.lines[0].spans;
        assert_eq!(spans[0], Span { text: "see ".into(), style: 0, target: None });
        assert_eq!(spans[1].text, "this ");
        assert_eq!(spans[2], Span { text: "link".into(), style: LINK | BOLD, target: Some(0) });
    }

    #[test]
    fn lists_nest() {
        let (t, _) = render("<ul><li>a<ul><li>b</li></ul></li></ul><ol start=3><li>c<li>d</ol>", 80);
        assert_eq!(t, "• a\n  ◦ b\n\n3. c\n4. d\n");
    }

    #[test]
    fn preformatted_and_long_words() {
        let (t, _) = render("<pre>a  b\n\tc\n\nd</pre><p>abcdefghijkl</p>", 10);
        assert_eq!(t, "a  b\n        c\n\nd\n\nabcdefghij\nkl\n");
    }

    #[test]
    fn tables_are_grids() {
        let (t, _) = render("<table border=1><tr><th>Name<th>Qty</tr><tr><td>apple<td>3</tr></table>", 80);
        assert_eq!(t, "Name  │ Qty\napple │ 3\n");
    }

    #[test]
    fn anchors_and_hidden() {
        let (t, l) = render(r#"<p>a</p><div hidden>x</div><p style="display: none">y</p><h2 id="s">S</h2>"#, 80);
        assert_eq!(t, "a\n\nS\n");
        assert_eq!(l.anchors.get("s"), Some(&2));
    }

    #[test]
    fn icon_links_get_labels() {
        let (t, _) = render(r#"<a href="/feed.xml"><img src=x></a> <a href="/" aria-label="Home"></a>"#, 80);
        assert_eq!(t, "[feed.xml] [Home]\n");
    }
}
