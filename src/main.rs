//! Lineal: a lightweight web browser for the terminal.

mod app;
mod editor;
mod engine;
mod net;
mod pages;
mod paint;
mod ui;

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, EventStream,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::net::{Outcome, Request};

const USAGE: &str = "\
Lineal - a lightweight web browser for the terminal

Usage: lineal [options] [URL | file | search terms...]

Options:
  -d, --dump         render the page and print it as plain text
      --dump-html    render the page and print it as an HTML file of terminal cells
  -s, --source       print the page source
  -w, --width N      columns for --dump (default: terminal width or 100)
  -h, --help         show this help
  -V, --version      show the version

Environment:
  LINEAL_SEARCH      search URL template, %s is replaced by the query
  LINEAL_HOME        start page when no URL is given (default about:home)

Press F1 inside Lineal for keys.";

#[derive(PartialEq)]
enum Output {
    Browser,
    Text,
    Html,
    Source,
}

struct Args {
    input: String,
    output: Output,
    width: Option<usize>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { input: String::new(), output: Output::Browser, width: None };
    let mut words = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("lineal {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-d" | "--dump" => args.output = Output::Text,
            "--dump-html" => args.output = Output::Html,
            "-s" | "--source" => args.output = Output::Source,
            "-w" | "--width" => {
                let n = it.next().ok_or("--width needs a number")?;
                args.width = Some(n.parse().map_err(|_| format!("bad width: {n}"))?);
            }
            "--" => words.extend(it.by_ref()),
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option: {s}")),
            _ => words.push(a),
        }
    }
    args.input = words.join(" ");
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lineal: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let search =
        std::env::var("LINEAL_SEARCH").ok().filter(|s| s.contains("%s")).unwrap_or(pages::DEFAULT_SEARCH.into());
    let input =
        if args.input.is_empty() { std::env::var("LINEAL_HOME").unwrap_or_default() } else { args.input.clone() };
    let req = app::resolve_input(&input, &search);

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("lineal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match args.output {
        Output::Browser => runtime.block_on(run(req, search)).map_err(|e| e.to_string()),
        _ => runtime.block_on(dump(req, &search, args.width, args.output)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lineal: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(start: Request, search: String) -> io::Result<()> {
    let client = net::client().map_err(io::Error::other)?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    // ratatui's hook (installed by `init`) restores the screen, then runs this one.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        previous(info);
    }));
    let mut terminal = ratatui::init();
    crossterm::execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;

    let mut app = app::App::new(client, tx, search);
    let size = terminal.size()?;
    app.set_viewport(0, 2, size.width as usize, size.height.saturating_sub(2) as usize);
    app.navigate(start);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    // Events update state right away; the (comparatively expensive) repaint happens at most once
    // per frame, so a burst of mouse moves doesn't queue up dozens of paints.
    const FRAME: Duration = Duration::from_millis(25);
    let mut last_draw = Instant::now() - FRAME;
    let mut pending = true;
    let result = loop {
        if pending && last_draw.elapsed() >= FRAME {
            if let Err(e) = terminal.draw(|f| ui::draw(f, &mut app)) {
                break Err(e);
            }
            last_draw = Instant::now();
            pending = false;
        }
        let wait = FRAME.saturating_sub(last_draw.elapsed());
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => {
                    app.on_event(ev);
                    pending = true;
                }
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            },
            Some(msg) = rx.recv() => {
                app.on_msg(msg);
                while let Ok(msg) = rx.try_recv() {
                    app.on_msg(msg);
                }
                pending = true;
            }
            _ = tokio::time::sleep(wait), if pending => {}
            _ = tick.tick(), if app.loading.is_some() || app.message.is_some() => {
                if app.message.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5)) {
                    app.message = None;
                }
                pending = true;
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

/// `--dump`, `--dump-html`, `--source`: load one page, wait for its stylesheets, print it.
async fn dump(mut req: Request, search: &str, width: Option<usize>, output: Output) -> Result<(), String> {
    let client = net::client().map_err(|e| e.to_string())?;
    let doc = loop {
        if req.url.scheme() == "about" {
            match pages::about(&req.url, search) {
                pages::About::Page(doc) => break doc,
                pages::About::Redirect(url) => req = Request::get(url),
            }
            continue;
        }
        match net::fetch(&client, &req).await? {
            Outcome::Doc(doc) => break doc,
            Outcome::Downloaded { path, bytes } => {
                eprintln!("Downloaded {} ({})", path.display(), app::human_size(bytes));
                return Ok(());
            }
        }
    };
    let mut out = io::stdout().lock();
    if output == Output::Source {
        return out.write_all(doc.text.as_bytes()).map_err(|e| e.to_string());
    }
    let cols = width.or_else(|| crossterm::terminal::size().ok().map(|(w, _)| w as usize)).unwrap_or(100);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let engine = engine::Engine::new(client, tx);
    let html = if doc.is_html() { doc.text.clone() } else { pages::plain_text(&doc.text) };
    let mut page = engine.page(&html, &doc.url, cols, 50);
    // Give stylesheets a few seconds to arrive.
    let deadline = Instant::now() + Duration::from_secs(10);
    while engine.pending() > 0 && Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let grid = page.paint_full(5000);
    let text = match output {
        Output::Html => grid_html(&grid),
        _ => grid.text(),
    };
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// The rendered cells as HTML, colors included (handy for sharing or checking a rendering).
fn grid_html(grid: &paint::Grid) -> String {
    let mut out = String::from(
        "<!doctype html><html><body style='background:#222;margin:0'><pre style='font:16px/16px DejaVu Sans Mono,monospace;margin:0'>",
    );
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let c = grid.get(col, row);
            let mut extra = String::new();
            if c.attrs & paint::attr::BOLD != 0 {
                extra += ";font-weight:bold";
            }
            if c.attrs & paint::attr::UNDERLINE != 0 {
                extra += ";text-decoration:underline";
            }
            if c.attrs & paint::attr::ITALIC != 0 {
                extra += ";font-style:italic";
            }
            let ch = match c.ch {
                '<' => "&lt;".to_string(),
                '>' => "&gt;".into(),
                '&' => "&amp;".into(),
                ch => ch.to_string(),
            };
            out += &format!(
                "<span style='color:rgb({},{},{});background:rgb({},{},{}){extra}'>{ch}</span>",
                c.fg[0], c.fg[1], c.fg[2], c.bg[0], c.bg[1], c.bg[2]
            );
        }
        out.push('\n');
    }
    out + "</pre></body></html>\n"
}
