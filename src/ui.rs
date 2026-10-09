//! Draws the app with ratatui: title bar, page, status/input line and popups.

use std::sync::OnceLock;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RLine, Span as RSpan};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Mode};
use crate::layout::style::*;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MESSAGE_TIME: Duration = Duration::from_secs(4);

fn color() -> bool {
    static COLOR: OnceLock<bool> = OnceLock::new();
    *COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()))
}

fn fg(c: Color) -> Style {
    if color() { Style::default().fg(c) } else { Style::default() }
}

pub fn span_style(bits: u16) -> Style {
    let mut s = Style::default();
    let color = color();
    if bits & BOLD != 0 {
        s = s.add_modifier(Modifier::BOLD);
    }
    if bits & ITALIC != 0 {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if bits & UNDERLINE != 0 {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if bits & STRIKE != 0 {
        s = s.add_modifier(Modifier::CROSSED_OUT);
    }
    if bits & DIM != 0 {
        s = if color { s.fg(Color::DarkGray) } else { s.add_modifier(Modifier::DIM) };
    }
    if bits & IMAGE != 0 {
        s = s.add_modifier(Modifier::ITALIC);
        s = if color { s.fg(Color::DarkGray) } else { s.add_modifier(Modifier::DIM) };
    }
    if color {
        if bits & CODE != 0 {
            s = s.fg(Color::Green);
        }
        if bits & H1 != 0 {
            s = s.fg(Color::Magenta).add_modifier(Modifier::UNDERLINED);
        } else if bits & H2 != 0 {
            s = s.fg(Color::Yellow);
        } else if bits & H3 != 0 {
            s = s.fg(Color::Blue);
        }
        if bits & MARK != 0 {
            s = s.fg(Color::Black).bg(Color::Yellow);
        }
        if bits & LINK != 0 {
            s = s.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED);
        }
        if bits & FIELD != 0 {
            s = s.fg(Color::White).bg(Color::DarkGray);
        }
    } else {
        if bits & (LINK | FIELD) != 0 {
            s = s.add_modifier(Modifier::UNDERLINED);
        }
        if bits & MARK != 0 {
            s = s.add_modifier(Modifier::REVERSED);
        }
    }
    s
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.height < 3 || area.width < 12 {
        f.render_widget(Paragraph::new("Lineal: window too small"), area);
        return;
    }
    let top = Rect { height: 1, ..area };
    let bottom = Rect { y: area.bottom() - 1, height: 1, ..area };
    // One column of margin on each side reads much better than text glued to the edge.
    let content = Rect { x: area.x + 1, y: area.y + 1, width: area.width - 2, height: area.height - 2 };
    app.set_viewport(content.x, content.y, content.width as usize, content.height as usize);

    draw_title(f, app, top);
    draw_page(f, app, content);
    draw_status(f, app, bottom);
    if let Mode::Select { field, cursor } = app.mode {
        draw_select(f, app, field, cursor, content);
    }
}

fn draw_title(f: &mut Frame, app: &App, area: Rect) {
    let bar = Style::default().add_modifier(Modifier::REVERSED);
    let right = match &app.loading {
        Some(l) => {
            let frame = (l.since.elapsed().as_millis() / 100) as usize % SPINNER.len();
            format!(" {} {} ", SPINNER[frame], l.url)
        }
        None => String::new(),
    };
    let right = truncate(&right, area.width as usize / 2);
    let left_w = (area.width as usize).saturating_sub(right.width());
    let title = if app.page.title.is_empty() { "Lineal".to_string() } else { app.page.title.clone() };
    let left = pad(&truncate(&format!(" {title}"), left_w), left_w);
    let line = RLine::from(vec![RSpan::styled(left, bar.add_modifier(Modifier::BOLD)), RSpan::styled(right, bar)]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_page(f: &mut Frame, app: &App, area: Rect) {
    let page = &app.page;
    let end = (page.scroll + area.height as usize).min(page.layout.lines.len());
    let lines: Vec<RLine> = page.layout.lines[page.scroll.min(end)..end]
        .iter()
        .map(|l| {
            RLine::from(
                l.spans
                    .iter()
                    .map(|s| {
                        let mut st = span_style(s.style);
                        if s.target.is_some() && s.target == page.focus {
                            st = st.add_modifier(Modifier::REVERSED);
                        }
                        RSpan::styled(s.text.as_str(), st)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);

    let buf = f.buffer_mut();
    let row_of = |line: usize| -> Option<u16> {
        (line >= page.scroll && line < page.scroll + area.height as usize).then(|| area.y + (line - page.scroll) as u16)
    };
    if let Some(find) = &app.find {
        for (i, &(line, col, w)) in find.matches.iter().enumerate() {
            let Some(y) = row_of(line) else { continue };
            let x = area.x + col as u16;
            if col >= area.width as usize {
                continue;
            }
            let w = w.min(area.width as usize - col) as u16;
            let style = if !color() {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if i == find.current {
                Style::default().fg(Color::Black).bg(Color::LightRed)
            } else {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            };
            buf.set_style(Rect { x, y, width: w, height: 1 }, style);
        }
    }
    if let Mode::Hints { hints, typed } = &app.mode {
        let style = if color() {
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        };
        for h in hints.iter().filter(|h| h.label.starts_with(typed.as_str())) {
            let Some(y) = row_of(h.line) else { continue };
            if h.col + h.label.len() > area.width as usize {
                continue;
            }
            buf.set_string(area.x + h.col as u16, y, &h.label, style);
        }
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    let input = |f: &mut Frame, prompt: &str, editor: &crate::editor::Editor| {
        let prompt = truncate(prompt, width / 2);
        let pw = prompt.width();
        let (text, cursor) = editor.view(width.saturating_sub(pw + 1));
        let line = RLine::from(vec![RSpan::styled(prompt, Style::default().add_modifier(Modifier::BOLD)), RSpan::raw(text)]);
        f.render_widget(Paragraph::new(line), area);
        f.set_cursor_position(Position { x: area.x + (pw + cursor) as u16, y: area.y });
    };
    match &app.mode {
        Mode::Url(e) => return input(f, "Go: ", e),
        Mode::Find(e) => return input(f, "/", e),
        Mode::Edit { field, editor } => return input(f, &format!("{}: ", app.field_name(*field)), editor),
        _ => {}
    }
    let lines = app.page.layout.lines.len();
    let pos = if lines <= app.height {
        "All".to_string()
    } else {
        let pct = (app.page.scroll + app.height).min(lines) * 100 / lines;
        format!("{pct}%")
    };
    let right = format!(" {pos}  ? help ");
    let left = match &app.mode {
        Mode::Hints { typed, .. } => format!("Follow: {typed}"),
        Mode::Select { .. } => "Pick with ↑↓ and Enter, Esc to cancel".into(),
        _ => match &app.message {
            Some((m, at)) if at.elapsed() < MESSAGE_TIME => m.clone(),
            _ => app.focus_description().unwrap_or_else(|| app.page.url().to_string()),
        },
    };
    let left_w = width.saturating_sub(right.width());
    let line = RLine::from(vec![
        RSpan::raw(pad(&truncate(&left, left_w), left_w)),
        RSpan::styled(right, fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_select(f: &mut Frame, app: &App, field: usize, cursor: usize, area: Rect) {
    let options = app.select_options(field);
    let w = options.iter().map(|o| o.width()).max().unwrap_or(0).clamp(10, area.width as usize - 4) as u16 + 4;
    let h = (options.len() as u16 + 2).min(area.height);
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    let items: Vec<ListItem> = options.iter().map(|o| ListItem::new(truncate(o, w as usize - 4))).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Choose "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(cursor));
    f.render_widget(Clear, rect);
    f.render_stateful_widget(list, rect, &mut state);
}

fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
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

fn pad(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(s.width())))
}
