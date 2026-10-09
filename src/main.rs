//! Lineal: a lightweight terminal web browser.

mod app;
mod css;
mod dom;
mod editor;
mod forms;
mod layout;
mod net;
mod pages;
mod ui;

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, EventStream,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::layout::Target;
use crate::net::{Outcome, Request};

const USAGE: &str = "\
Lineal - a lightweight terminal web browser

Usage: lineal [options] [URL | file | search terms...]

Options:
  -d, --dump         print the rendered page as text and exit
  -s, --source       print the page source and exit
  -w, --width N      text width for --dump (default: terminal width or 80)
  -h, --help         show this help
  -V, --version      show the version

Environment:
  LINEAL_SEARCH      search URL template, %s is replaced by the query
  LINEAL_HOME        start page when no URL is given (default about:home)
  NO_COLOR           disable colors

Press ? inside Lineal for key bindings.";

struct Args {
    input: String,
    dump: bool,
    source: bool,
    width: Option<usize>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { input: String::new(), dump: false, source: false, width: None };
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
            "-d" | "--dump" => args.dump = true,
            "-s" | "--source" => args.source = true,
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
    let result = if args.dump || args.source {
        runtime.block_on(dump(req, &search, args.width, args.source))
    } else {
        runtime.block_on(run(req, search)).map_err(|e| e.to_string())
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
    app.navigate(start, true);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let result = loop {
        if let Err(e) = terminal.draw(|f| ui::draw(f, &mut app)) {
            break Err(e);
        }
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => app.on_event(ev),
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            },
            Some(msg) = rx.recv() => app.on_msg(msg),
            _ = tick.tick(), if app.loading.is_some() || app.message.is_some() => {
                if app.message.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5)) {
                    app.message = None;
                }
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

/// `--dump` / `--source`: fetch one page and print it.
async fn dump(mut req: Request, search: &str, width: Option<usize>, source: bool) -> Result<(), String> {
    let doc = loop {
        if req.url.scheme() == "about" {
            match pages::about(&req.url, search) {
                pages::About::Page(doc) => break doc,
                pages::About::Redirect(url) => req = Request::get(url),
            }
            continue;
        }
        let client = net::client().map_err(|e| e.to_string())?;
        match net::fetch(&client, &req).await? {
            Outcome::Doc(doc) => break doc,
            Outcome::Downloaded { path, bytes } => {
                eprintln!("Saved {} ({})", path.display(), app::human_size(bytes));
                return Ok(());
            }
        }
    };
    let mut out = io::stdout().lock();
    if source {
        return out.write_all(doc.text.as_bytes()).map_err(|e| e.to_string());
    }
    let width = width.or_else(|| crossterm::terminal::size().ok().map(|(w, _)| w as usize)).unwrap_or(80);
    let text = if doc.is_html() {
        let dom = dom::Dom::parse(&doc.text, &doc.url, &doc.hidden);
        let layout = dom.layout(width, true);
        let mut text = layout.plain_text();
        let links: Vec<String> = layout
            .targets
            .iter()
            .enumerate()
            .filter_map(|(i, t)| match t {
                Target::Link(u) => Some(format!("[{}] {u}", i + 1)),
                Target::Field(_) => None,
            })
            .collect();
        if !links.is_empty() {
            text += "\nLinks:\n";
            text += &links.join("\n");
            text.push('\n');
        }
        text
    } else {
        layout::Layout::text(&doc.text, width).plain_text()
    };
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}
