# Lineal

A lightweight web browser for the terminal that shows websites the way a real browser does.

Lineal doesn't strip pages down to text like lynx or w3m. It runs a real CSS engine (Stylo, the
one inside Firefox) and a real layout engine (Taffy: block, flexbox, grid, floats), then paints
the result into terminal cells: page colors and backgrounds, columns and sidebars, boxes and
borders, bold/italic/underlined text, all inside Firefox-style browser chrome (tab strip, back /
forward / reload buttons, address bar). It is still one small native binary: about 9-11 MB, a few
tens of MB of memory, no browser or Chromium underneath.

Built with [tokio](https://tokio.rs), [ratatui](https://ratatui.rs) and
[Blitz](https://github.com/DioxusLabs/blitz) (Stylo + Taffy + Parley), plus rustls for HTTPS.
Pure Rust apart from `ring`'s crypto.

> **Status: early alpha.** No JavaScript yet, images show as placeholders, one tab.

## Features

- Real CSS: colors, backgrounds, gradients, flexbox, grid, floats, tables, `@media` queries
- Firefox-like chrome: tab strip, ← → ↻ buttons, address bar that searches or opens URLs
- Mouse: click links, buttons and fields, hover to see where links go, wheel scrolling
- Keyboard: Tab through links and fields (highlighted), Enter to follow, Firefox shortcuts
- Forms: type into text fields, submit with Enter or buttons, GET and POST
- History with instant back/forward, fragment links, downloads to `~/Downloads`
- `--dump` renders any page as text; `--dump-html` exports the colored rendering as HTML
- Fully static Linux binaries (no fontconfig, no OpenSSL)

## Install

Download a build from the [releases](https://github.com/Zexolver/lineal-browser/releases), or:

```sh
cargo install --git https://github.com/Zexolver/lineal-browser
```

Builds are made for Linux (x86_64, aarch64, armv7, riscv64, i686; static musl), Windows (x86_64,
aarch64) and macOS (Intel, Apple silicon). A terminal with 24-bit color is recommended (almost all
modern ones: Windows Terminal, kitty, WezTerm, foot, GNOME Terminal, Konsole, iTerm2, Alacritty…).

## Usage

```sh
lineal                       # start page
lineal example.org           # a site (https:// is assumed)
lineal rust terminal ui      # anything that isn't a URL is a web search
lineal ./page.html           # local files
lineal --dump news.ycombinator.com
```

| Key                         | Action                                     |
|-----------------------------|--------------------------------------------|
| Ctrl-L, Alt-D, F6           | edit the address (URL or search)           |
| Tab / Shift-Tab, Enter      | move between links and fields, activate    |
| ↑ ↓, PgUp PgDn, Space       | scroll                                     |
| Home / End                  | top / bottom                               |
| Alt-← / Alt-→, Backspace    | back / forward                             |
| F5, Ctrl-R                  | reload                                     |
| Esc                         | stop loading, leave a text field           |
| F1                          | help                                       |
| Ctrl-Q, Ctrl-C              | quit                                       |

Environment: `LINEAL_SEARCH` (search URL with `%s`), `LINEAL_HOME` (start page).

## How it works

```
 address bar ─► net.rs (reqwest + rustls, tokio) ─► engine.rs: Blitz document
                                                     (html5ever → Stylo CSS → Taffy layout)
 Blitz's painter (blitz-paint) ─► paint.rs: a PaintScene for terminals ─► cells ─► ui.rs (ratatui)
```

`paint.rs` receives the same drawing commands a GPU renderer would and splits them up:

- **text runs** become real terminal characters in their CSS color (glyph ids are mapped back to
  characters through each font's character map);
- **thin lines** (borders, underlines, carets) become underlines or `▔ ▁ ▏ ▕` line characters;
- **everything else** (backgrounds, rounded boxes, gradients) is rasterized on the CPU and averaged
  into cell colors, using `▀` half blocks where a cell's top and bottom differ.

To make pages fit the character grid, Lineal's built-in stylesheet gives all text one monospace
font (bundled DejaVu Sans Mono) at one size and line height. Then every line of text gets its own
terminal row, and gaps between lines only get rows when they're at least about half a row tall.
Without this, pixel spacing on real pages would produce random blank lines.

## Roadmap

- [ ] Images: half-block pixels everywhere, real images on kitty/sixel/iTerm2 terminals (can be turned off)
- [ ] JavaScript via Boa (pure Rust), as an optional feature
- [ ] Tabs, find in page, bookmarks
- [ ] Show any script even without its fonts installed
- [ ] Termux / Android

## License

GPL-3.0-or-later (see [LICENSE](LICENSE)). The bundled DejaVu Sans Mono font is under the Bitstream
Vera license with public-domain DejaVu changes (see `assets/fonts/LICENSE-DejaVu.txt`).
