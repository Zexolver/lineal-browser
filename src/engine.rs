//! The page engine: a Blitz document (Stylo CSS + Taffy layout) wired to Lineal's network stack,
//! history and terminal painter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use blitz_dom::{BaseDocument, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_traits::events::{
    BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta, BlitzWheelEvent, KeyState, MouseEventButton,
    MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::{Body, Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
use keyboard_types::{Code, Key, Location, Modifiers};
use parley::fontique::{Blob, Collection, CollectionOptions, GenericFamily, SourceCache};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::app::Msg;
use crate::paint::{CELL_H, CELL_W, Grid, Painter, RowMap};

/// The monospace font every page is drawn with, so text lands exactly on the character grid.
const FONT_REGULAR: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono-Bold.ttf");
const FONT_FAMILY: &str = "DejaVu Sans Mono";

/// Keyboard focus highlight (a terminal can't show a thin focus ring well).
const FOCUS_BG: &str = "#ffd75e";

/// Biggest subresource (stylesheet, image…) we download.
const MAX_RESOURCE: usize = 10 * 1024 * 1024;

/// Shared by all pages: fonts, network and the channel back to the app.
pub struct Engine {
    font_ctx: parley::FontContext,
    net: Arc<Net>,
    nav: Arc<Nav>,
    shell: Arc<Shell>,
    ua_css: String,
}

impl Engine {
    pub fn new(client: reqwest::Client, tx: UnboundedSender<Msg>) -> Engine {
        let mut font_ctx = parley::FontContext {
            source_cache: SourceCache::new_shared(),
            collection: Collection::new(CollectionOptions { shared: true, system_fonts: true }),
        };
        let mut ours = Vec::new();
        for font in [FONT_REGULAR, FONT_BOLD] {
            let families = font_ctx.collection.register_fonts(Blob::new(Arc::new(font) as _), None);
            ours.extend(families.into_iter().map(|(id, _)| id));
        }
        ours.dedup();
        font_ctx.collection.register_fonts(Blob::new(Arc::new(blitz_dom::BULLET_FONT) as _), None);
        #[cfg(target_os = "linux")]
        crate::fonts::register_fallbacks(&mut font_ctx.collection);
        // Our font comes first for every generic family too, so text that doesn't go through the
        // page stylesheet (form fields use a built-in `sans-serif`) still lands on the grid.
        // System fonts stay behind it as fallbacks for scripts it doesn't cover.
        for generic in [
            GenericFamily::SansSerif,
            GenericFamily::Serif,
            GenericFamily::Monospace,
            GenericFamily::SystemUi,
            GenericFamily::UiSansSerif,
            GenericFamily::UiSerif,
            GenericFamily::UiMonospace,
            GenericFamily::Cursive,
            GenericFamily::Fantasy,
        ] {
            let rest: Vec<_> = font_ctx.collection.generic_families(generic).filter(|f| !ours.contains(f)).collect();
            font_ctx.collection.set_generic_families(generic, ours.iter().copied().chain(rest));
        }
        // Pick the font size whose advance is exactly one cell wide.
        let advance = skrifa::FontRef::new(FONT_REGULAR)
            .ok()
            .and_then(|f| {
                use skrifa::MetadataProvider;
                let upem = f
                    .metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default())
                    .units_per_em as f64;
                let gid = f.charmap().map('M')?;
                let adv = f
                    .glyph_metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default())
                    .advance_width(gid)? as f64;
                Some(adv / upem)
            })
            .unwrap_or(0.602);
        let font_size = CELL_W / advance;
        // User-agent `!important` rules beat everything a page says (CSS cascade), which is how we
        // keep every page on the grid: one font, one size, one line height.
        // Tables use separate borders: Blitz's collapsed-border painting draws lines between all
        // rows even when a table has no borders, and 1-2px spacing is invisible in a terminal anyway.
        let ua_css = format!(
            "*, *::before, *::after, input, textarea, select, button {{ \
                font-family: '{FONT_FAMILY}' !important; font-size: {font_size:.4}px !important; \
                line-height: {CELL_H}px !important; letter-spacing: 0 !important; word-spacing: 0 !important; }} \
             a:focus, button:focus, select:focus, [tabindex]:focus, input[type=submit]:focus, \
             input[type=button]:focus, input[type=checkbox]:focus, input[type=radio]:focus {{ \
                background-color: {FOCUS_BG} !important; color: #15141a !important; outline: none !important; }} \
             input:focus, textarea:focus {{ outline: none !important; }} \
             table {{ border-collapse: separate !important; border-spacing: 0 !important; }}"
        );
        Engine {
            font_ctx,
            net: Arc::new(Net {
                client,
                rt: tokio::runtime::Handle::current(),
                pending: Arc::new(AtomicUsize::new(0)),
                cache: Arc::new(Mutex::new(HashMap::new())),
            }),
            nav: Arc::new(Nav { tx: tx.clone() }),
            shell: Arc::new(Shell { tx }),
            ua_css,
        }
    }

    /// Subresource fetches still in flight.
    pub fn pending(&self) -> usize {
        self.net.pending.load(Ordering::Relaxed)
    }

    pub fn page(&self, html: &str, url: &Url, cols: usize, rows: usize) -> Page {
        let (w, h) = viewport_px(cols, rows);
        let config = DocumentConfig {
            viewport: Some(Viewport::new(w, h, 1.0, ColorScheme::Light)),
            base_url: Some(url.to_string()),
            ua_stylesheets: Some(vec![self.ua_css.clone()]),
            net_provider: Some(self.net.clone()),
            navigation_provider: Some(self.nav.clone()),
            shell_provider: Some(self.shell.clone()),
            font_ctx: Some(self.font_ctx.clone()),
            ..Default::default()
        };
        let mut doc = HtmlDocument::from_html(html, config);
        doc.resolve(0.0);
        let mut page = Page { doc, painter: Painter::default(), map: RowMap::default(), cols, rows };
        if let Some(fragment) = url.fragment() {
            page.doc.scroll_to_fragment(fragment);
        }
        page
    }
}

fn viewport_px(cols: usize, rows: usize) -> (u32, u32) {
    ((cols as f64 * CELL_W) as u32, (rows as f64 * CELL_H) as u32)
}

/// One loaded page.
pub struct Page {
    pub doc: HtmlDocument,
    painter: Painter,
    /// Row layout of the last paint, to turn screen rows back into page positions.
    map: RowMap,
    cols: usize,
    rows: usize,
}

impl Page {
    pub fn title(&self) -> String {
        self.doc
            .find_title_node()
            .map(|n| n.text_content().split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        (self.cols, self.rows) = (cols, rows);
        let (w, h) = viewport_px(cols, rows);
        self.doc.set_viewport(Viewport::new(w, h, 1.0, ColorScheme::Light));
    }

    /// Applies loaded resources and re-runs style and layout.
    pub fn update(&mut self) {
        self.doc.handle_messages();
        self.doc.resolve(0.0);
    }

    pub fn paint(&mut self) -> Grid {
        self.update();
        let doc: &mut BaseDocument = &mut self.doc;
        let mut grid = self.painter.paint(doc, self.cols, self.rows);
        image_placeholders(&self.doc, &mut grid);
        field_placeholders(&self.doc, &mut grid);
        self.map = grid.map.clone();
        grid
    }

    /// Paints the whole page (not just the viewport), for `--dump`.
    pub fn paint_full(&mut self, max_rows: usize) -> Grid {
        self.update();
        let cols = self.cols;
        let mut out = Grid { cols, rows: 0, cells: Vec::new(), map: RowMap::default() };
        let mut y = 0.0;
        loop {
            self.scroll_to(y);
            let top = self.doc.viewport_scroll().y;
            let grid = self.paint();
            // Near the end, scrolling stops early; skip rows we already have.
            for (row, &(y0, _)) in grid.map.ranges.iter().enumerate() {
                if top + y0 >= y - 0.5 && out.rows < max_rows {
                    out.cells.extend_from_slice(&grid.cells[row * cols..(row + 1) * cols]);
                    out.rows += 1;
                }
            }
            let next = top + grid.map.end();
            if next <= y + 1.0 || next >= self.content_height() || out.rows >= max_rows {
                break;
            }
            y = next;
        }
        out
    }

    pub fn content_height(&self) -> f64 {
        let root = self.doc.root_element();
        root.final_layout().size.height.max(root.scroll_height()) as f64
    }

    /// (scroll position, viewport height, content height) in px, for the scroll indicator.
    pub fn scroll_info(&self) -> (f64, f64, f64) {
        (self.doc.viewport_scroll().y, self.rows as f64 * CELL_H, self.content_height())
    }

    pub fn scroll_by(&mut self, rows: f64) {
        self.doc.scroll_viewport_by(0.0, rows * CELL_H);
    }

    pub fn scroll_to(&mut self, y: f64) {
        let max = (self.content_height() - self.rows as f64 * CELL_H).max(0.0);
        self.doc.set_viewport_scroll(blitz_dom::Point { x: 0.0, y: y.clamp(0.0, max) });
    }

    pub fn scroll_to_fragment(&mut self, fragment: &str) -> bool {
        self.doc.scroll_to_fragment(fragment)
    }

    /// Viewport y (px) of the middle of a screen row.
    fn row_y(&self, row: usize) -> f64 {
        self.map.center(row).unwrap_or((row as f64 + 0.5) * CELL_H)
    }

    fn coords(&self, col: usize, row: usize) -> PointerCoords {
        let x = (col as f64 + 0.5) * CELL_W;
        let y = self.row_y(row);
        let scroll = self.doc.viewport_scroll();
        PointerCoords {
            page_x: (x + scroll.x) as f32,
            page_y: (y + scroll.y) as f32,
            screen_x: x as f32,
            screen_y: y as f32,
            client_x: x as f32,
            client_y: y as f32,
        }
    }

    fn pointer(&self, col: usize, row: usize, pressed: bool) -> BlitzPointerEvent {
        BlitzPointerEvent {
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: self.coords(col, row),
            button: MouseEventButton::Main,
            buttons: if pressed { MouseEventButtons::Primary } else { MouseEventButtons::None },
            mods: Modifiers::empty(),
            details: PointerDetails::default(),
            element: Default::default(),
            active_pointers: Default::default(),
        }
    }

    fn send(&mut self, event: UiEvent) {
        let mut driver = blitz_dom::EventDriver::new(&mut self.doc, blitz_dom::NoopEventHandler);
        driver.handle_ui_event(event);
    }

    pub fn mouse_move(&mut self, col: usize, row: usize) {
        let e = self.pointer(col, row, false);
        self.send(UiEvent::PointerMove(e));
    }

    /// A full click (down + up) at a cell. Links and form buttons end up in `Msg::Navigate`.
    pub fn click(&mut self, col: usize, row: usize) {
        let down = self.pointer(col, row, true);
        self.send(UiEvent::PointerDown(down));
        let up = self.pointer(col, row, false);
        self.send(UiEvent::PointerUp(up));
    }

    pub fn wheel(&mut self, col: usize, row: usize, lines: f64) {
        let coords = self.coords(col, row);
        self.send(UiEvent::Wheel(BlitzWheelEvent {
            delta: BlitzWheelDelta::Pixels(0.0, -lines * CELL_H),
            coords,
            buttons: MouseEventButtons::None,
            mods: Modifiers::empty(),
            element: Default::default(),
        }));
    }

    /// The link under a cell, if any.
    pub fn link_at(&self, col: usize, row: usize) -> Option<Url> {
        let x = ((col as f64 + 0.5) * CELL_W) as f32;
        let y = self.row_y(row) as f32;
        let mut id = self.doc.element_from_point(x, y);
        while let Some(node_id) = id {
            let node = self.doc.get_node(node_id)?;
            if tag(node) == Some("a")
                && let Some(href) = node.attr(blitz_dom::local_name!("href"))
            {
                return self.doc.base_url().join(href.trim()).ok();
            }
            id = node.parent;
        }
        None
    }

    /// Whether keyboard input should go to the page (a text field has focus).
    pub fn wants_keys(&self) -> bool {
        let Some(id) = self.doc.get_focussed_node_id() else { return false };
        let Some(node) = self.doc.get_node(id) else { return false };
        tag(node) == Some("textarea")
            || (tag(node) == Some("input")
                && !matches!(
                    node.attr(blitz_dom::local_name!("type")).map(|t| t.to_ascii_lowercase()).as_deref(),
                    Some("checkbox" | "radio" | "submit" | "button" | "reset" | "image" | "hidden" | "file")
                ))
    }

    /// Where the focused link points, if a link has keyboard focus.
    pub fn focused_link(&self) -> Option<Url> {
        let node = self.doc.get_node(self.doc.get_focussed_node_id()?)?;
        if tag(node) != Some("a") {
            return None;
        }
        self.doc.base_url().join(node.attr(blitz_dom::local_name!("href"))?.trim()).ok()
    }

    pub fn has_focus(&self) -> bool {
        self.doc.get_focussed_node_id().is_some()
    }

    pub fn focus_next(&mut self, forward: bool) -> bool {
        let moved = if self.doc.get_focussed_node_id().is_none() {
            // Start from the top of what's on screen.
            self.first_focusable()
        } else if forward {
            self.doc.focus_next_node()
        } else {
            self.doc.focus_prev_node()
        };
        if let Some(id) = moved {
            self.reveal(id);
        }
        moved.is_some()
    }

    fn first_focusable(&mut self) -> Option<blitz_dom::NodeId> {
        let top = self.doc.viewport_scroll().y as f32;
        let mut stack = vec![self.doc.root_node().id];
        let mut first = None;
        while let Some(id) = stack.pop() {
            let node = self.doc.get_node(id)?;
            if node.is_focussable() {
                let y = node.absolute_position(0.0, 0.0).y;
                if y >= top {
                    first = Some(id);
                    break;
                }
                first.get_or_insert(id);
            }
            stack.extend(node.children.iter().rev().copied());
        }
        let id = first?;
        self.doc.set_focus_to(id);
        Some(id)
    }

    /// Scrolls so the node is visible.
    fn reveal(&mut self, id: blitz_dom::NodeId) {
        let Some(node) = self.doc.get_node(id) else { return };
        let y = node.absolute_position(0.0, 0.0).y as f64;
        let h = node.final_layout().size.height as f64;
        let (top, view) = (self.doc.viewport_scroll().y, self.rows as f64 * CELL_H);
        if y < top || y + h > top + view {
            self.scroll_to(y - view / 3.0);
        }
    }

    /// Enter on the focused element: follow a link, press a button, or let a text field handle it.
    pub fn activate_focused(&mut self) {
        let Some(id) = self.doc.get_focussed_node_id() else { return };
        if self.wants_keys() {
            self.key(Key::Enter, None, Modifiers::empty());
            return;
        }
        let Some(node) = self.doc.get_node(id) else { return };
        let pos = node.absolute_position(0.0, 0.0);
        let size = node.final_layout().size;
        let scroll = self.doc.viewport_scroll();
        let col = ((pos.x as f64 + (size.width as f64 / 2.0).min(CELL_W)) / CELL_W).floor().max(0.0) as usize;
        let mid = pos.y as f64 - scroll.y + size.height as f64 / 2.0;
        let row = self.map.locate(mid).map_or(0, |(r, _)| r);
        self.click(col.min(self.cols.saturating_sub(1)), row.min(self.rows.saturating_sub(1)));
    }

    pub fn blur(&mut self) {
        self.doc.clear_focus();
    }

    /// Sends a key to the focused element.
    pub fn key(&mut self, key: Key, text: Option<&str>, modifiers: Modifiers) {
        for state in [KeyState::Pressed, KeyState::Released] {
            let event = BlitzKeyEvent {
                key: key.clone(),
                code: Code::Unidentified,
                modifiers,
                location: Location::Standard,
                is_auto_repeating: false,
                is_composing: false,
                state,
                text: text.map(Into::into),
            };
            self.send(if state == KeyState::Pressed { UiEvent::KeyDown(event) } else { UiEvent::KeyUp(event) });
        }
    }
}

/// The element's tag name, if it is an element.
fn tag(node: &blitz_dom::Node) -> Option<&str> {
    node.element_data().map(|e| e.name.local.as_ref())
}

/// Images aren't drawn yet: show a box with the alt text where each one sits.
fn image_placeholders(doc: &BaseDocument, grid: &mut Grid) {
    let scroll = doc.viewport_scroll();
    let mut stack = vec![doc.root_node().id];
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else { continue };
        stack.extend(node.children.iter().copied());
        if tag(node) != Some("img") {
            continue;
        }
        let size = node.final_layout().size;
        if size.width < CELL_W as f32 || size.height < 4.0 {
            continue;
        }
        let pos = node.absolute_position(0.0, 0.0);
        let x0 = (pos.x as f64 / CELL_W).round().max(0.0) as usize;
        let x1 = (((pos.x + size.width) as f64 / CELL_W).round() as usize).min(grid.cols);
        let (top, bottom) = (pos.y as f64 - scroll.y, pos.y as f64 + size.height as f64 - scroll.y);
        if bottom <= 0.0 || top >= grid.map.end() || x0 >= x1 {
            continue;
        }
        let (Some((y0, _)), Some((y1, _))) = (grid.map.locate(top.max(0.0)), grid.map.locate(bottom - 1.0)) else {
            continue;
        };
        let y1 = y1 + 1;
        let alt = node.attr(blitz_dom::local_name!("alt")).unwrap_or("").trim().to_string();
        // Never cover text: small images (icons, avatars) often overlap a line's row slightly.
        let free = |c: &crate::paint::Cell| matches!(c.ch, ' ' | '▀');
        for row in y0..y1.max(y0 + 1).min(grid.rows) {
            for col in x0..x1 {
                let cell = &mut grid.cells[row * grid.cols + col];
                if free(cell) {
                    *cell = crate::paint::Cell { ch: ' ', fg: [90, 90, 100], bg: [228, 228, 234], attrs: 0 };
                }
            }
        }
        if alt.is_empty() || y1 - y0 < 1 {
            continue;
        }
        let row = y0 + (y1.saturating_sub(y0)) / 2;
        if row < grid.rows {
            for (i, ch) in alt.chars().take(x1 - x0).enumerate() {
                let cell = &mut grid.cells[row * grid.cols + x0 + i];
                if cell.bg == [228, 228, 234] {
                    cell.ch = ch;
                }
            }
        }
    }
}

/// Blitz doesn't draw `placeholder` hints, so we do: in gray, inside empty text fields.
fn field_placeholders(doc: &BaseDocument, grid: &mut Grid) {
    let scroll = doc.viewport_scroll();
    let mut stack = vec![doc.root_node().id];
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else { continue };
        stack.extend(node.children.iter().copied());
        if !matches!(tag(node), Some("input" | "textarea")) {
            continue;
        }
        let Some(hint) = node.attr(blitz_dom::local_name!("placeholder")).map(str::trim).filter(|h| !h.is_empty())
        else {
            continue;
        };
        let pos = node.absolute_position(0.0, 0.0);
        let layout = node.final_layout();
        let x0 = ((pos.x + layout.border.left + layout.padding.left) as f64 / CELL_W).ceil().max(0.0) as usize;
        let x1 = (((pos.x + layout.size.width - layout.border.right - layout.padding.right) as f64 / CELL_W).floor()
            as usize)
            .min(grid.cols);
        // The text row: middle of the box.
        let mid = pos.y as f64 + layout.size.height as f64 / 2.0 - scroll.y;
        if mid < 0.0 || mid >= grid.map.end() || x0 >= x1 {
            continue;
        }
        let Some((row, _)) = grid.map.locate(mid) else { continue };
        let cells = &mut grid.cells[row * grid.cols + x0..row * grid.cols + x1];
        if cells.iter().any(|c| !matches!(c.ch, ' ' | '▀' | '▁' | '▔' | '▏' | '▕')) {
            continue; // has a value (or the caret)
        }
        for (cell, ch) in cells.iter_mut().zip(hint.chars()) {
            cell.ch = ch;
            cell.fg = [117, 117, 128];
            cell.attrs = 0;
        }
    }
}

// ---- providers ------------------------------------------------------------------------------

struct Net {
    client: reqwest::Client,
    rt: tokio::runtime::Handle,
    pending: Arc<AtomicUsize>,
    /// Recently fetched subresources, so going back and forth doesn't refetch stylesheets.
    cache: Arc<Mutex<HashMap<Url, Bytes>>>,
}

/// Resources we skip for now: fonts (we always use our own) and images (not drawn yet).
fn skipped(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    let ext = path.rsplit('.').next().unwrap_or("");
    matches!(
        ext,
        "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "eot"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "svg"
            | "ico"
            | "bmp"
    )
}

impl NetProvider for Net {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url.clone();
        if skipped(&url) {
            return;
        }
        if let Some(bytes) = self.cache.lock().unwrap().get(&url).cloned() {
            self.rt.spawn_blocking(move || handler.bytes(url.to_string(), bytes));
            return;
        }
        let (client, pending, cache) = (self.client.clone(), self.pending.clone(), self.cache.clone());
        pending.fetch_add(1, Ordering::Relaxed);
        self.rt.spawn(async move {
            let result = match url.scheme() {
                "http" | "https" => fetch_resource(&client, &request).await.map(|(u, b)| (u.to_string(), b)),
                "data" => data_url(url.as_str()).map(|b| (url.to_string(), b)),
                "file" => match url.to_file_path() {
                    Ok(p) => tokio::fs::read(p).await.ok().map(|b| (url.to_string(), Bytes::from(b))),
                    Err(_) => None,
                },
                _ => None,
            };
            if let Some((final_url, bytes)) = result {
                {
                    let mut c = cache.lock().unwrap();
                    if c.len() > 200 {
                        c.clear();
                    }
                    c.insert(url, bytes.clone());
                }
                // Parsing CSS can take a while; keep it off the UI thread.
                let _ = tokio::task::spawn_blocking(move || handler.bytes(final_url, bytes)).await;
            }
            pending.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn fetch_resource(client: &reqwest::Client, req: &Request) -> Option<(Url, Bytes)> {
    let mut builder = client.request(req.method.clone(), req.url.clone()).headers(req.headers.clone());
    if let Body::Bytes(b) = &req.body {
        builder = builder.body(b.clone());
    }
    let mut resp = builder.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let url = resp.url().clone();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_RESOURCE {
            return None;
        }
    }
    Some((url, Bytes::from(body)))
}

/// Decodes a `data:` URL.
fn data_url(url: &str) -> Option<Bytes> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if meta.ends_with(";base64") {
        base64_decode(data).map(Bytes::from)
    } else {
        let decoded: Vec<u8> = url::form_urlencoded::parse(format!("x={}", data.replace('+', "%2B")).as_bytes())
            .next()?
            .1
            .into_owned()
            .into();
        Some(Bytes::from(decoded))
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'%' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

struct Nav {
    tx: UnboundedSender<Msg>,
}

impl NavigationProvider for Nav {
    fn navigate_to(&self, options: NavigationOptions) {
        let _ = self.tx.send(Msg::Navigate(options));
    }
}

struct Shell {
    tx: UnboundedSender<Msg>,
}

impl ShellProvider for Shell {
    fn request_redraw(&self) {
        let _ = self.tx.send(Msg::Redraw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls() {
        assert_eq!(data_url("data:text/css;base64,Ym9keXt9").as_deref(), Some(&b"body{}"[..]));
        assert_eq!(data_url("data:text/css,a%20b").as_deref(), Some(&b"a b"[..]));
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn typing_into_a_field() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = Engine::new(crate::net::client().unwrap(), tx);
        let url = Url::parse("about:blank").unwrap();
        let mut page = engine.page("<input name=q>", &url, 40, 10);
        assert!(page.focus_next(true), "focus moved");
        eprintln!("focused: {:?} wants_keys={}", page.doc.get_focussed_node_id(), page.wants_keys());
        page.key(Key::Character("h".into()), Some("h"), Modifiers::empty());
        page.key(Key::Character("i".into()), Some("i"), Modifiers::empty());
        let id = page.doc.get_focussed_node_id().unwrap();
        let value = page
            .doc
            .get_node(id)
            .unwrap()
            .element_data()
            .unwrap()
            .text_input_data()
            .map(|d| d.editor.raw_text().to_string());
        eprintln!("value: {value:?}");
        let grid = page.paint();
        let text = grid.text();
        assert!(text.contains("hi"), "{text:?}");
    }
}

#[cfg(test)]
mod submit_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn enter_submits_forms() {
        for action in ["https://example.org/s", "https://lineal.invalid/search"] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let engine = Engine::new(crate::net::client().unwrap(), tx);
            let url = Url::parse("about:home").unwrap();
            let html = format!("<form action=\"{action}\"><input name=q><button>Go</button></form>");
            let mut page = engine.page(&html, &url, 40, 10);
            page.focus_next(true);
            page.key(Key::Character("x".into()), Some("x"), Modifiers::empty());
            page.key(Key::Enter, None, Modifiers::empty());
            let mut got = None;
            while let Ok(msg) = rx.try_recv() {
                if let Msg::Navigate(o) = msg {
                    got = Some(o.url.to_string());
                }
            }
            assert_eq!(got, Some(format!("{action}?q=x")));
        }
    }
}
