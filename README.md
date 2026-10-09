# Lineal

A lightweight, modern terminal web browser written in Rust.

Lineal gives you real web browsing from a terminal: search, readable pages, working links and
forms, history and find, in one small native binary (about 4 MB) that starts instantly and
uses a few MB of RAM. It is built on [tokio](https://tokio.rs), [ratatui](https://ratatui.rs),
[html5ever](https://github.com/servo/html5ever) (Servo's HTML parser) and
[rustls](https://github.com/rustls/rustls).

```
 Hacker News
 [news.ycombinator.com]   Hacker News new | past | comments | ask | show | jobs | submit    login

 1.   Cloudflare acquires Deno (deno.com)
      839 points by ilreb 6 hours ago | hide | 463 comments
 2.   Our $445M Series D (oxide.computer)
      471 points by ahlCVA 6 hours ago | hide | 191 comments
https://news.ycombinator.com/                                                     4%  ? help
```

## Features

- **Spec-compliant HTML parsing** (html5ever), charset detection, gzip/brotli/deflate, HTTP/2, cookies.
- **Text layout engine** built for terminals: wrapping, headings, lists, quotes, code blocks,
  real column-based **tables** (Wikipedia infoboxes, Hacker News), `<noscript>` content.
- **Knows what pages hide**: applies `display:none`/`visibility:hidden` rules from inline and
  external stylesheets (with `@media` evaluation), the `hidden` attribute and screen-reader-only classes,
  so you don't see collapsed menus, modals and duplicated mobile markup.
- **Forms**: text fields, passwords, checkboxes, radio buttons, selects, GET and POST submission.
- **Keyboard first, mouse friendly**: Tab between links, vimium-style link hints (`f`), vim keys,
  click links and fields, scroll with the wheel.
- History with instant back/forward (pages are cached), find in page, view source, fragment links,
  `<meta refresh>`, downloads to `~/Downloads`, local files and directory listings.
- Search from the address bar (DuckDuckGo Lite by default; configurable).
- Copy URLs to your clipboard over OSC 52 (works over SSH).
- `--dump` mode prints any page as text with numbered links, for scripts and pipes.
- Respects `NO_COLOR` and uses your terminal's own color palette.

## Install

Download a build for your platform from the [releases](https://github.com/Zexolver/lineal-browser/releases),
or build from source with a recent stable Rust:

```sh
cargo install --git https://github.com/Zexolver/lineal-browser
# or
git clone https://github.com/Zexolver/lineal-browser && cd lineal-browser && cargo build --release
```

Release builds are produced by CI for:

| OS      | Architectures                                   | Notes                     |
|---------|-------------------------------------------------|---------------------------|
| Linux   | x86_64, aarch64, armv7, riscv64, i686           | static (musl), no deps    |
| Windows | x86_64, aarch64                                 | Windows Terminal recommended |
| macOS   | x86_64 (Intel), aarch64 (Apple silicon)         |                           |

Android via Termux is not officially supported yet, but `cargo install` inside Termux should work.

## Usage

```sh
lineal                          # start page
lineal example.org              # open a site (https:// is assumed)
lineal rust terminal ui         # anything that isn't a URL is a web search
lineal ./notes.html             # local files and directories
lineal --dump news.ycombinator.com | less    # render as text and exit
```

| Key                              | Action                                   |
|----------------------------------|------------------------------------------|
| `o` / `O`                        | open URL or search / edit current URL    |
| `Tab` / `Shift-Tab`, `Enter`     | move between links and fields, activate  |
| `f`                              | link hints: type the label to follow     |
| `j` `k` `↓` `↑`, `Space` `b`, `Ctrl-d` `Ctrl-u` | scroll                    |
| `g` / `G`                        | top / bottom                             |
| `H` / `L`, `Backspace`, `Alt-←` / `Alt-→` | back / forward                  |
| `/`, `n` / `N`                   | find in page, next / previous match      |
| `r`                              | reload                                   |
| `y` / `Y`                        | copy page URL / focused link URL         |
| `\`                              | toggle page source                       |
| `?`                              | help                                     |
| `q`                              | quit                                     |

Environment variables: `LINEAL_SEARCH` (search URL template with `%s`, e.g.
`https://search.marginalia.nu/search?query=%s`), `LINEAL_HOME` (start page), `NO_COLOR`.

## How it works, and why this design

```
 URL bar ─► net.rs (reqwest + rustls/ring, tokio) ─► dom.rs (html5ever DOM, forms, hidden set)
                                    │                         ▲
                                    └─► css.rs: external sheets, "what's hidden" only
 dom ─► layout.rs (blocks, inline flow, wrapping, tables) ─► lines of styled spans ─► ui.rs (ratatui)
```

The goal is a browser that is **light, fast, portable and as pure-Rust as possible**. Options considered:

- **Gosub engine**: interesting, but pre-alpha, with a large dependency surface and a graphical
  pipeline we would mostly throw away in a terminal.
- **Servo / Blitz (Stylo + Taffy)**: a full CSS engine. Correct, but it lays out in pixels,
  is heavy to build and run, and a terminal can't show most of what it computes.
- **Our own text-mode engine on html5ever** ← chosen. We parse with a browser-grade parser, then lay
  out *for a character grid*: block/inline flow, word wrap and tables computed in columns, not pixels.
  CSS is used only for what matters in a terminal (is this element hidden?). This keeps the binary
  around 4 MB with no C dependencies apart from `ring`'s crypto.

**JavaScript** is the obvious next question. A JS engine alone is not enough; sites need a DOM,
events, timers and `fetch`, which is most of the work. The plan is an optional `js` cargo feature
using [Boa](https://boajs.dev) (pure Rust) with a minimal DOM binding over the same tree, so the
default build stays small and fast, and you can opt into JS when you need it.

## Roadmap

- [ ] Optional JavaScript (Boa) behind a cargo feature, with a small DOM API
- [ ] Tabs, bookmarks, persistent history and cookies
- [ ] Inline images on terminals that support them (kitty, sixel, iTerm2)
- [ ] Reader mode (extract the main article)
- [ ] Config file for keys, colors and search engines
- [ ] Termux / Android support

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
