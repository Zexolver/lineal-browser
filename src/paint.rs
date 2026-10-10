//! Paints a Blitz document into a grid of terminal cells.
//!
//! Blitz's own painter (`blitz-paint`) drives this: it walks the laid-out page and issues 2D drawing
//! commands. We implement its `PaintScene` interface and split the commands three ways:
//!
//! * text runs become real terminal characters (glyph ids are mapped back to characters through the
//!   font's character map), in the CSS text color;
//! * thin horizontal/vertical lines (borders, underlines, text carets) become underlines or
//!   eighth-block line characters, because averaged into a cell they would just vanish;
//! * everything else (backgrounds, gradients, rounded boxes, shadows) is rasterized by `vello_cpu`
//!   into a small pixel canvas, which then gives each cell its colors, using half-block characters
//!   where the top and bottom of a cell differ.

use std::collections::HashMap;
use std::sync::Arc;

use anyrender::{Filter, Glyph, NormalizedCoord, Paint, PaintRef, PaintScene, RenderContext};
use anyrender_vello_cpu::VelloCpuScenePainter;
use blitz_dom::BaseDocument;
use kurbo::{Affine, Point, Rect, Shape, Stroke};
use peniko::{BlendMode, Fill, FontData, StyleRef};
use skrifa::MetadataProvider;

/// Size of one terminal cell in CSS pixels. Cells are about twice as tall as wide.
pub const CELL_W: f64 = 8.0;
pub const CELL_H: f64 = 16.0;

/// Lines thinner than this (in CSS px) are drawn as line characters instead of being rasterized.
const THIN: f64 = 3.0;

pub mod attr {
    pub const BOLD: u8 = 1;
    pub const ITALIC: u8 = 2;
    pub const UNDERLINE: u8 = 4;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub attrs: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: [0, 0, 0], bg: [255, 255, 255], attrs: 0 }
    }
}

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    /// Which part of the page each row shows.
    pub map: RowMap,
}

impl Grid {
    pub fn get(&self, col: usize, row: usize) -> &Cell {
        &self.cells[row * self.cols + col]
    }

    /// The visible text, one line per row (for `--dump`).
    pub fn text(&self) -> String {
        let mut s = String::new();
        for row in 0..self.rows {
            let line: String = (0..self.cols).map(|c| self.get(c, row).ch).collect();
            let line = line.replace(['▀', '▔', '▁', '▏', '▕'], " ");
            s += line.trim_end();
            s.push('\n');
        }
        s.trim_end_matches('\n').to_string() + "\n"
    }
}

#[derive(Clone, Copy)]
struct Text {
    /// Glyph origin x (CSS px).
    x: f64,
    ch: char,
    rgb: [u8; 3],
    attrs: u8,
    /// Baseline position (CSS px).
    baseline: f64,
}

#[derive(Clone, Copy)]
struct Line {
    /// Start and end along the line, and its position across it (CSS px, viewport space).
    from: f64,
    to: f64,
    at: f64,
    rgb: [u8; 3],
}

/// Per-font info: glyph id -> char, and whether the face is bold.
type FontInfo = (HashMap<u32, char>, bool);

struct TermScene<'a> {
    raster: VelloCpuScenePainter,
    cols: usize,
    height: f64,
    text: Vec<Text>,
    hlines: Vec<Line>,
    vlines: Vec<Line>,
    clips: Vec<Rect>,
    fonts: &'a mut HashMap<(u64, u32), FontInfo>,
}

impl TermScene<'_> {
    fn clip(&self) -> Rect {
        self.clips.last().copied().unwrap_or(Rect::new(f64::MIN, f64::MIN, f64::MAX, f64::MAX))
    }

    /// Records thin solid lines; returns false if the shape should be rasterized instead.
    fn thin_line(&mut self, transform: Affine, paint: &PaintRef<'_>, shape: &impl Shape, stroke_w: f64) -> bool {
        let Paint::Solid(color) = paint else { return false };
        let c = color.to_rgba8();
        if c.a < 40 {
            return true; // nearly invisible: drop it
        }
        // Only axis-aligned shapes keep their shape under a bounding box.
        if transform.as_coeffs()[1] != 0.0 || transform.as_coeffs()[2] != 0.0 {
            return false;
        }
        let b = transform.transform_rect_bbox(shape.bounding_box().inflate(stroke_w / 2.0, stroke_w / 2.0));
        let b = b.intersect(self.clip());
        if b.is_zero_area() {
            return true;
        }
        let rgb = [c.r, c.g, c.b];
        if b.height() <= THIN && b.width() >= CELL_W * 0.75 {
            self.hlines.push(Line { from: b.x0, to: b.x1, at: (b.y0 + b.y1) / 2.0, rgb });
            true
        } else if b.width() <= THIN && b.height() >= CELL_H * 0.4 {
            self.vlines.push(Line { from: b.y0, to: b.y1, at: (b.x0 + b.x1) / 2.0, rgb });
            true
        } else {
            false
        }
    }
}

impl RenderContext for TermScene<'_> {}

impl PaintScene for TermScene<'_> {
    fn reset(&mut self) {
        self.raster.reset();
        self.text.clear();
        self.hlines.clear();
        self.vlines.clear();
        self.clips.clear();
    }

    fn push_layer(
        &mut self,
        blend: impl Into<BlendMode>,
        alpha: f32,
        transform: Affine,
        clip: &impl Shape,
        filter: Option<Arc<Filter>>,
        backdrop_filter: Option<Arc<Filter>>,
    ) {
        let r = transform.transform_rect_bbox(clip.bounding_box()).intersect(self.clip());
        self.clips.push(r);
        self.raster.push_layer(blend, alpha, transform, clip, filter, backdrop_filter);
    }

    fn push_clip_layer(&mut self, transform: Affine, clip: &impl Shape) {
        let r = transform.transform_rect_bbox(clip.bounding_box()).intersect(self.clip());
        self.clips.push(r);
        self.raster.push_clip_layer(transform, clip);
    }

    fn pop_layer(&mut self) {
        self.clips.pop();
        self.raster.pop_layer();
    }

    fn stroke<'b>(
        &mut self,
        style: &Stroke,
        transform: Affine,
        brush: impl Into<PaintRef<'b>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        let brush = brush.into();
        if style.width <= THIN && self.thin_line(transform, &brush, shape, style.width) {
            return;
        }
        self.raster.stroke(style, transform, brush, brush_transform, shape);
    }

    fn fill<'b>(
        &mut self,
        style: Fill,
        transform: Affine,
        brush: impl Into<PaintRef<'b>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        let brush = brush.into();
        if self.thin_line(transform, &brush, shape, 0.0) {
            return;
        }
        self.raster.fill(style, transform, brush, brush_transform, shape);
    }

    fn draw_glyphs<'b, 's: 'b>(
        &'s mut self,
        font: &'b FontData,
        _font_size: f32,
        _hint: bool,
        _normalized_coords: &'b [NormalizedCoord],
        embolden: kurbo::Vec2,
        _style: impl Into<StyleRef<'b>>,
        brush: impl Into<PaintRef<'b>>,
        brush_alpha: f32,
        transform: Affine,
        glyph_transform: Option<Affine>,
        glyphs: impl Iterator<Item = Glyph> + Clone,
    ) {
        let rgb = match brush.into() {
            Paint::Solid(c) => {
                let c = c.to_rgba8();
                if c.a < 30 || brush_alpha < 0.1 {
                    return; // invisible text
                }
                [c.r, c.g, c.b]
            }
            _ => [0, 0, 0],
        };
        let clip = self.clip();
        let (map, bold) = self.fonts.entry((font.data.id(), font.index)).or_insert_with(|| font_info(font));
        let mut attrs = 0;
        if *bold || embolden.x > 0.0 {
            attrs |= attr::BOLD;
        }
        // Synthesized italics come as a skew.
        if glyph_transform.is_some_and(|t| t.as_coeffs()[2] != 0.0) {
            attrs |= attr::ITALIC;
        }
        for g in glyphs {
            let p = transform * Point::new(g.x as f64, g.y as f64);
            // Test the middle of the glyph, not its origin: text often starts exactly on a clip edge.
            if !clip.contains(Point::new(p.x + CELL_W / 2.0, p.y - CELL_H / 3.0)) {
                continue;
            }
            // `p` is the glyph origin on the baseline.
            if p.x < -CELL_W / 2.0 || p.x >= self.cols as f64 * CELL_W || p.y < 0.0 || p.y > self.height + CELL_H {
                continue;
            }
            let ch = map.get(&g.id).copied().unwrap_or('\u{fffd}');
            self.text.push(Text { x: p.x, ch, rgb, attrs, baseline: p.y });
        }
    }

    fn draw_box_shadow(&mut self, _transform: Affine, _rect: Rect, _brush: peniko::Color, _radius: f64, _std_dev: f64) {
        // Soft shadows are invisible at terminal resolution (and the CPU rasterizer paints
        // them as solid blocks), so they're skipped.
    }
}

fn font_info(font: &FontData) -> FontInfo {
    let mut map = HashMap::new();
    let mut bold = false;
    if let Ok(f) = skrifa::FontRef::from_index(font.data.data(), font.index) {
        for (cp, gid) in f.charmap().mappings() {
            if let Some(c) = char::from_u32(cp) {
                map.entry(gid.to_u32()).or_insert(c);
            }
        }
        bold = f.attributes().weight.value() >= 600.0;
    }
    (map, bold)
}

/// Caches what we learn about fonts between frames.
#[derive(Default)]
pub struct Painter {
    fonts: HashMap<(u64, u32), FontInfo>,
}

impl Painter {
    /// Paints the document's current viewport into a `cols` x `rows` grid.
    pub fn paint(&mut self, doc: &mut BaseDocument, cols: usize, rows: usize) -> Grid {
        // Paint more than a screenful of page: dropping the thin gaps between lines of text
        // (see `RowMap`) usually makes the page shorter than its pixel height.
        let w = (cols as f64 * CELL_W) as u32;
        let h = ((rows as f64 * CELL_H * 1.5) as u32).clamp(1, 32000);
        let mut scene = TermScene {
            raster: VelloCpuScenePainter::new(w.max(1) as u16, h as u16),
            cols,
            height: h as f64,
            text: Vec::new(),
            hlines: Vec::new(),
            vlines: Vec::new(),
            clips: Vec::new(),
            fonts: &mut self.fonts,
        };
        // Browsers paint a white canvas under the page.
        scene.raster.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            peniko::Color::WHITE,
            None,
            &Rect::new(0.0, 0.0, w as f64, h as f64),
        );
        blitz_paint::paint_scene(&mut scene, doc, 1.0, w, h, 0, 0);
        let text = std::mem::take(&mut scene.text);
        let (hlines, vlines) = (std::mem::take(&mut scene.hlines), std::mem::take(&mut scene.vlines));
        let pixmap = scene.raster.finish();
        let canvas = Canvas { pixels: pixmap.data(), width: w as usize, height: h as usize };
        compose(cols, rows, &canvas, text, &hlines, &vlines)
    }
}

/// Maps terminal rows to vertical ranges of the page (CSS px, from the top of the viewport).
///
/// Web pages put text lines at arbitrary pixel offsets (a 16px line, then a 5px gap…), which a
/// uniform 16px-per-row grid turns into randomly appearing blank rows. Instead every line of text
/// gets exactly one row, and the space between lines gets as many rows as it is tall, rounded,
/// so thin gaps disappear while real spacing, boxes and pictures keep their size.
#[derive(Clone, Debug, Default)]
pub struct RowMap {
    /// Source pixel range of each row.
    pub ranges: Vec<(f64, f64)>,
}

impl RowMap {
    /// Builds the map from the baselines of the text on screen.
    fn new(baselines: &[f64], rows: usize, height: f64) -> (RowMap, Vec<(f64, f64, usize)>) {
        // Group baselines into lines; sub/superscripts and table cells a few px off share a row.
        let mut lines: Vec<(f64, f64)> = Vec::new(); // (first baseline, last baseline)
        for &b in baselines {
            match lines.last_mut() {
                Some(l) if b - l.0 < CELL_H / 2.0 => l.1 = b,
                _ => lines.push((b, b)),
            }
        }
        let mut ranges = Vec::with_capacity(rows);
        let mut line_rows = Vec::with_capacity(lines.len()); // (first baseline, last baseline, row)
        let mut y = 0.0;
        let gap_rows = |ranges: &mut Vec<(f64, f64)>, from: f64, to: f64| {
            let n = ((to - from) / CELL_H + 0.35).floor().max(0.0) as usize;
            for i in 0..n {
                let step = (to - from) / n as f64;
                ranges.push((from + step * i as f64, from + step * (i + 1) as f64));
            }
        };
        for (first, last) in lines {
            if ranges.len() >= rows {
                break;
            }
            let (top, bottom) = ((first - 12.0).max(y), (last + 4.0).max(y + 1.0));
            if top > y {
                gap_rows(&mut ranges, y, top);
            }
            if ranges.len() >= rows {
                break;
            }
            line_rows.push((first, last, ranges.len()));
            ranges.push((top, bottom));
            y = bottom;
        }
        while ranges.len() < rows && y < height {
            let to = (y + CELL_H).min(height);
            ranges.push((y, to));
            y = to;
        }
        ranges.truncate(rows);
        (RowMap { ranges }, line_rows)
    }

    /// Where a page y (px) lands: (row, fraction within the row). Points in dropped gaps snap
    /// to the edge of the nearest row.
    pub fn locate(&self, y: f64) -> Option<(usize, f64)> {
        let i = self.ranges.partition_point(|r| r.1 <= y);
        let (y0, y1) = *self.ranges.get(i)?;
        if y >= y0 {
            return Some((i, (y - y0) / (y1 - y0).max(1e-6)));
        }
        // In a gap before row i.
        match i.checked_sub(1) {
            Some(prev) if y - self.ranges[prev].1 < y0 - y => Some((prev, 0.99)),
            _ => Some((i, 0.0)),
        }
    }

    /// The page y (px) in the middle of a row, for mouse hit testing.
    pub fn center(&self, row: usize) -> Option<f64> {
        self.ranges.get(row).map(|r| (r.0 + r.1) / 2.0)
    }

    /// How far down the page the last row reaches (px).
    pub fn end(&self) -> f64 {
        self.ranges.last().map_or(0.0, |r| r.1)
    }
}

struct Canvas<'a> {
    pixels: &'a [peniko::color::PremulRgba8],
    width: usize,
    height: usize,
}

impl Canvas<'_> {
    /// Average color of a pixel rectangle (clamped to the canvas).
    fn avg(&self, x0: usize, y0: f64, x1: usize, y1: f64) -> [u8; 3] {
        let y0 = (y0.max(0.0) as usize).min(self.height.saturating_sub(1));
        let y1 = (y1.ceil().max(0.0) as usize).clamp(y0 + 1, self.height.max(1));
        let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
        for y in y0..y1 {
            for p in &self.pixels[y * self.width + x0..y * self.width + x1] {
                // Premultiplied: composite over white, so anything left transparent shows as paper.
                let white = 255 - p.a as u32;
                r += p.r as u32 + white;
                g += p.g as u32 + white;
                b += p.b as u32 + white;
                n += 1;
            }
        }
        let n = n.max(1);
        [(r / n) as u8, (g / n) as u8, (b / n) as u8]
    }
}

fn compose(cols: usize, rows: usize, canvas: &Canvas, mut text: Vec<Text>, hlines: &[Line], vlines: &[Line]) -> Grid {
    text.sort_by(|a, b| a.baseline.total_cmp(&b.baseline));
    let baselines: Vec<f64> = text.iter().map(|t| t.baseline).collect();
    let (map, line_rows) = RowMap::new(&baselines, rows, canvas.height as f64);
    let rows = map.ranges.len().max(1);

    let mut cells = Vec::with_capacity(cols * rows);
    for row in 0..rows {
        let (y0, y1) = map.ranges.get(row).copied().unwrap_or((canvas.height as f64, canvas.height as f64 + CELL_H));
        let ym = (y0 + y1) / 2.0;
        for col in 0..cols {
            let (x0, x1) = ((col as f64 * CELL_W) as usize, ((col + 1) as f64 * CELL_W) as usize);
            let (top, bottom) = (canvas.avg(x0, y0, x1, ym), canvas.avg(x0, ym, x1, y1));
            cells.push(if distance(top, bottom) < 24 {
                Cell { ch: ' ', fg: top, bg: canvas.avg(x0, y0, x1, y1), attrs: 0 }
            } else {
                Cell { ch: '▀', fg: top, bg: bottom, attrs: 0 }
            });
        }
    }
    // Text goes on its line's row, over the background right behind the glyph's body.
    let mut placed: Vec<(usize, f64, &Text)> = Vec::with_capacity(text.len());
    let mut line = 0;
    for t in &text {
        while line < line_rows.len() && t.baseline > line_rows[line].1 {
            line += 1;
        }
        let Some(&(_, _, row)) = line_rows.get(line) else { break };
        placed.push((row, t.x, t));
    }
    // Left to right along each row, never putting a glyph on the column of the one before it:
    // proportional fonts (fallbacks, form fields) and sub-pixel drift would otherwise overwrite.
    placed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let mut last: Option<(usize, usize)> = None; // (row, col)
    for (row, x, t) in placed {
        // The small bias keeps glyphs sitting a hair before a half cell on the same side.
        let mut col = (x / CELL_W + 0.1).round().max(0.0) as usize;
        if let Some((r, c)) = last
            && r == row
            && col <= c
        {
            col = c + 1;
        }
        if col >= cols {
            continue;
        }
        last = Some((row, col));
        if t.ch == ' ' {
            continue;
        }
        let (x0, x1) = ((col as f64 * CELL_W) as usize, ((col + 1) as f64 * CELL_W) as usize);
        let qx = (x1 - x0) / 4;
        let bg = canvas.avg(x0 + qx, t.baseline - 9.0, x1 - qx, t.baseline - 1.0);
        cells[row * cols + col] = Cell { ch: t.ch, fg: t.rgb, bg, attrs: t.attrs };
    }
    let mut grid = Grid { cols, rows, cells, map };
    draw_lines(&mut grid, hlines, vlines);
    grid
}

fn distance(a: [u8; 3], b: [u8; 3]) -> u32 {
    a.iter().zip(&b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs()).sum()
}

fn is_text(c: &Cell) -> bool {
    !matches!(c.ch, ' ' | '▀' | '▔' | '▁' | '▏' | '▕' | '─' | '│')
}

fn draw_lines(grid: &mut Grid, hlines: &[Line], vlines: &[Line]) {
    for l in hlines {
        let Some((row, frac)) = grid.map.locate(l.at) else { continue };
        let c0 = (l.from / CELL_W).round().max(0.0) as usize;
        let c1 = ((l.to / CELL_W).round() as usize).min(grid.cols);
        if c0 >= c1 {
            continue;
        }
        // A line in the lower half of a row that has text over it is an underline. Any other line
        // through a row of text (a box edge that lands there) would just chop up the gaps between
        // words, so it's left out.
        let text_fg = (c0..c1).map(|c| grid.get(c, row)).find(|c| is_text(c)).map(|c| c.fg);
        // Real underlines are drawn in the text color (`currentColor`); anything else under text
        // is a background sliver or box edge.
        let underline = frac > 0.5 && text_fg.is_some_and(|fg| distance(fg, l.rgb) < 90);
        if text_fg.is_some() && !underline {
            continue;
        }
        for c in c0..c1 {
            let cell = &mut grid.cells[row * grid.cols + c];
            if underline {
                cell.attrs |= attr::UNDERLINE;
            } else if !is_text(cell) && distance(cell.bg, l.rgb) > 40 {
                cell.ch = if frac < 0.3 {
                    '▔'
                } else if frac > 0.7 {
                    '▁'
                } else {
                    '─'
                };
                cell.fg = l.rgb;
            }
        }
    }
    for l in vlines {
        let col = (l.at / CELL_W).floor();
        if col < 0.0 || col as usize >= grid.cols {
            continue;
        }
        let col = col as usize;
        let frac = l.at / CELL_W - col as f64;
        let (Some((r0, _)), Some((r1, _))) = (grid.map.locate(l.from + 1.0), grid.map.locate(l.to - 1.0)) else {
            continue;
        };
        for r in r0..=r1.max(r0).min(grid.rows - 1) {
            let cell = &mut grid.cells[r * grid.cols + col];
            if !is_text(cell) && !matches!(cell.ch, '▔' | '▁' | '─') && distance(cell.bg, l.rgb) > 40 {
                cell.ch = if frac < 0.35 {
                    '▏'
                } else if frac > 0.65 {
                    '▕'
                } else {
                    '│'
                };
                cell.fg = l.rgb;
            }
        }
    }
}
