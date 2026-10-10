//! Draws the browser: Firefox-style tab strip and toolbar, the page, and status popups.

use std::sync::OnceLock;
use std::time::Duration;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Mode, Toolbar};
use crate::paint::{Grid, attr};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MESSAGE_TIME: Duration = Duration::from_secs(4);

// Firefox's dark theme.
const TABSTRIP_BG: [u8; 3] = [28, 27, 34];
const TAB_BG: [u8; 3] = [66, 65, 77];
const TOOLBAR_BG: [u8; 3] = [43, 42, 51];
const FIELD_BG: [u8; 3] = [28, 27, 34];
const FIELD_FOCUS_BG: [u8; 3] = [66, 65, 77];
const TEXT: [u8; 3] = [251, 251, 254];
const DIM: [u8; 3] = [140, 140, 155];
const DISABLED: [u8; 3] = [90, 90, 102];
const ACCENT: [u8; 3] = [0, 221, 255];

/// How colors are sent to the terminal.
#[derive(Clone, Copy, PartialEq)]
enum Palette {
    TrueColor,
    Ansi256,
}

fn palette() -> Palette {
    static P: OnceLock<Palette> = OnceLock::new();
    *P.get_or_init(|| {
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        let apple = std::env::var("TERM_PROGRAM").is_ok_and(|t| t == "Apple_Terminal");
        if apple && !colorterm.contains("truecolor") && !colorterm.contains("24bit") {
            Palette::Ansi256
        } else {
            Palette::TrueColor
        }
    })
}

pub fn rgb(c: [u8; 3]) -> Color {
    match palette() {
        Palette::TrueColor => Color::Rgb(c[0], c[1], c[2]),
        Palette::Ansi256 => Color::Indexed(ansi256(c)),
    }
}

/// Nearest xterm-256 color.
fn ansi256(c: [u8; 3]) -> u8 {
    let level = |v: u8| if v < 48 { 0 } else if v < 115 { 1 } else { (v as u16 - 35) / 40 } as u8;
    let (r, g, b) = (level(c[0]), level(c[1]), level(c[2]));
    let cube = 16 + 36 * r + 6 * g + b;
    let to_v = |l: u8| if l == 0 { 0 } else { 55 + 40 * l as i32 };
    let cube_err: i32 = [(c[0], r), (c[1], g), (c[2], b)].iter().map(|&(v, l)| (v as i32 - to_v(l)).abs()).sum();
    let avg = (c[0] as i32 + c[1] as i32 + c[2] as i32) / 3;
    let gray = ((avg - 8).max(0) / 10).min(23);
    let gray_v = 8 + gray * 10;
    let gray_err: i32 = c.iter().map(|&v| (v as i32 - gray_v).abs()).sum();
    if gray_err < cube_err { 232 + gray as u8 } else { cube }
}

fn style(fg: [u8; 3], bg: [u8; 3]) -> Style {
    Style::default().fg(rgb(fg)).bg(rgb(bg))
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.height < 4 || area.width < 20 {
        f.buffer_mut().set_string(0, 0, "Lineal: window too small", Style::default());
        return;
    }
    let content = Rect { x: area.x, y: area.y + 2, width: area.width, height: area.height - 2 };
    app.set_viewport(content.x, content.y, content.width as usize, content.height as usize);

    draw_tabs(f.buffer_mut(), app, Rect { height: 1, ..area });
    let cursor = draw_toolbar(f.buffer_mut(), app, Rect { y: area.y + 1, height: 1, ..area });
    match app.grid() {
        Some(grid) => blit(f.buffer_mut(), grid, content),
        None => f.buffer_mut().set_style(content, style([0, 0, 0], [255, 255, 255])),
    }
    draw_scrollbar(f.buffer_mut(), app, content);
    draw_status(f.buffer_mut(), app, content);
    if let Some(pos) = cursor {
        f.set_cursor_position(pos);
    }
}

fn fill(buf: &mut Buffer, area: Rect, bg: [u8; 3]) {
    buf.set_style(area, Style::default().bg(rgb(bg)));
    for x in area.left()..area.right() {
        for y in area.top()..area.bottom() {
            buf[(x, y)].set_symbol(" ");
        }
    }
}

/// Writes `s` clipped to `max` columns; returns the columns used.
fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, max: usize, st: Style) -> usize {
    let s = truncate(s, max);
    buf.set_stringn(x, y, &s, max, st);
    s.width()
}

fn draw_tabs(buf: &mut Buffer, app: &App, area: Rect) {
    fill(buf, area, TABSTRIP_BG);
    let tab_w = (area.width as usize).clamp(12, 34).min(area.width as usize - 4);
    let tab = Rect { x: area.x + 1, width: tab_w as u16, ..area };
    fill(buf, tab, TAB_BG);
    let icon = match &app.loading {
        Some(l) => SPINNER[(l.since.elapsed().as_millis() / 100) as usize % SPINNER.len()],
        None => "◆",
    };
    let title = if app.title.is_empty() { "New Tab" } else { &app.title };
    buf.set_string(tab.x + 1, tab.y, icon, style(if app.loading.is_some() { ACCENT } else { DIM }, TAB_BG));
    put(buf, tab.x + 3, tab.y, title, tab_w.saturating_sub(6), style(TEXT, TAB_BG));
    buf.set_string(tab.right() - 2, tab.y, "×", style(DIM, TAB_BG));
    buf.set_string(tab.right() + 1, tab.y, "+", style(DIM, TABSTRIP_BG));
}

/// Draws back/forward/reload and the address field. Returns where the text cursor goes when editing.
fn draw_toolbar(buf: &mut Buffer, app: &mut App, area: Rect) -> Option<Position> {
    fill(buf, area, TOOLBAR_BG);
    let y = area.y;
    let button = |enabled: bool| style(if enabled { TEXT } else { DISABLED }, TOOLBAR_BG);
    buf.set_string(area.x + 1, y, "←", button(app.can_go_back()));
    buf.set_string(area.x + 4, y, "→", button(app.can_go_forward()));
    buf.set_string(area.x + 7, y, if app.loading.is_some() { "×" } else { "↻" }, button(true));
    let field = Rect { x: area.x + 10, y, width: area.width.saturating_sub(13), height: 1 };
    app.toolbar = Toolbar {
        row: y,
        back: (area.x, area.x + 3),
        forward: (area.x + 3, area.x + 6),
        reload: (area.x + 6, area.x + 9),
        address: (field.x, field.right()),
    };
    let editing = matches!(app.mode, Mode::Address(_));
    let bg = if editing { FIELD_FOCUS_BG } else { FIELD_BG };
    fill(buf, field, bg);
    buf.set_string(area.right() - 2, y, "≡", button(true));
    let inner_w = field.width.saturating_sub(2) as usize;
    if let Mode::Address(editor) = &app.mode {
        let (text, cursor) = editor.view(inner_w);
        let st = if editor.selected { style(TEXT, [0, 96, 223]) } else { style(TEXT, bg) };
        buf.set_stringn(field.x + 1, y, &text, inner_w, st);
        return Some(Position { x: field.x + 1 + cursor as u16, y });
    }
    // Like Firefox: the host stands out, the rest of the URL is dimmed.
    let url = &app.url;
    if url.scheme() == "about" && matches!(url.path(), "home" | "newtab" | "blank") {
        put(buf, field.x + 1, y, "Search or enter address", inner_w, style(DIM, bg));
        return None;
    }
    let full = url.as_str();
    let (pre, host, rest) = match url.host_str().and_then(|h| full.find(h).map(|i| (i, h.len()))) {
        Some((i, n)) => (&full[..i], &full[i..i + n], &full[i + n..]),
        None => ("", full, ""),
    };
    let mut x = field.x + 1;
    let mut left = inner_w;
    for (part, color) in [(pre, DIM), (host, TEXT), (rest, DIM)] {
        let used = put(buf, x, y, part, left, style(color, bg));
        x += used as u16;
        left -= used.min(left);
    }
    None
}

/// Copies the painted page into the terminal buffer.
fn blit(buf: &mut Buffer, grid: &Grid, area: Rect) {
    for row in 0..grid.rows.min(area.height as usize) {
        let mut skip = 0;
        for col in 0..grid.cols.min(area.width as usize) {
            let cell = &mut buf[(area.x + col as u16, area.y + row as u16)];
            cell.reset();
            let g = grid.get(col, row);
            if skip > 0 {
                skip -= 1;
                cell.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
                continue;
            }
            let mut st = style(g.fg, g.bg);
            if g.attrs & attr::BOLD != 0 {
                st = st.add_modifier(Modifier::BOLD);
            }
            if g.attrs & attr::ITALIC != 0 {
                st = st.add_modifier(Modifier::ITALIC);
            }
            if g.attrs & attr::UNDERLINE != 0 {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
            let w = g.ch.width().unwrap_or(0);
            if w == 0 || (w == 2 && col + 1 >= area.width as usize) {
                cell.set_char(' ').set_style(st);
                continue;
            }
            cell.set_char(g.ch).set_style(st);
            skip = w - 1;
        }
    }
}

fn draw_scrollbar(buf: &mut Buffer, app: &App, area: Rect) {
    let Some((top, bottom)) = app.scroll_fraction() else { return };
    let h = area.height as f64;
    let (y0, y1) = ((top * h).floor() as u16, ((bottom * h).ceil() as u16).max((top * h).floor() as u16 + 1));
    let x = area.right() - 1;
    for y in y0..y1.min(area.height) {
        let cell = &mut buf[(x, area.y + y)];
        cell.set_symbol("▐").set_fg(rgb([150, 150, 160]));
    }
}

/// Firefox shows link targets and short messages in a popup at the bottom-left.
fn draw_status(buf: &mut Buffer, app: &App, area: Rect) {
    let text = match &app.message {
        Some((m, at)) if at.elapsed() < MESSAGE_TIME => m.clone(),
        _ => match (&app.hover, &app.loading) {
            (Some(url), _) => url.to_string(),
            (None, Some(l)) => format!("Loading {}…", l.url),
            _ => return,
        },
    };
    let max = (area.width as usize * 2 / 3).max(20).min(area.width as usize);
    let text = truncate(&format!(" {text} "), max);
    let y = area.bottom() - 1;
    let st = style([21, 20, 26], [240, 240, 244]);
    buf.set_string(area.x, y, &text, st);
}

fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_quantization() {
        assert_eq!(ansi256([0, 0, 0]), 16);
        assert_eq!(ansi256([255, 255, 255]), 231);
        assert_eq!(ansi256([255, 0, 0]), 196);
        assert_eq!(ansi256([128, 128, 128]), 244);
    }
}
