//! vmi-tui: terminal UI over vmi-mcp panes (single view + switcher).
//!
//! Level 1 of the TUI plan: one focused pane fills the main area, a
//! status bar lists every pane (focus mark, alive, idle, attached),
//! keystrokes route to the focused pane. The VT byte stream from
//! pane_read(data_hex) is parsed into a per-pane screen grid (vte);
//! ratatui renders the grid. DSR cursor queries are ANSWERED HERE —
//! synthesized from the grid state — so full-screen remote apps work
//! without a real terminal behind us.
//!
//! Keys (Ctrl-B prefix, same convention as vmi-attach):
//!   Ctrl-B n/p  next/prev pane     Ctrl-B 0-9  jump by index
//!   Ctrl-B c    open pane          Ctrl-B x    kill focused pane
//!   Ctrl-B q    quit
//! Anything else (including bare Ctrl-N etc.) is forwarded verbatim.
//!
//! --self-test drives one pane headless and prints the rendered grid:
//! exits 0 when the marker shows up (CI/pipe-friendly verification).

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Cell as UICell;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use rmcp::model::{CallToolRequestParams, CallToolResponse, ClientConfig, JsonObject};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
};
use serde_json::{json, Value};
use tokio_stream::StreamExt;
use unicode_width::UnicodeWidthChar;

type Client = RunningService<RoleClient, ClientConfig>;

async fn call(svc: &Client, name: &str, args: JsonObject) -> Result<Value> {
    let params = CallToolRequestParams::new(name.to_owned()).with_arguments(args);
    let result = match svc.call_tool_once(params).await? {
        CallToolResponse::Complete(r) => r,
        _ => bail!("tool {name}: unexpected response"),
    };
    if result.is_error.unwrap_or(false) {
        let text = result
            .content
            .first()
            .map(|c| format!("{c:?}"))
            .unwrap_or_default();
        bail!("tool {name} failed: {text}")
    }
    Ok(result.structured_content.clone().unwrap_or(Value::Null))
}

fn obj(v: Value) -> JsonObject {
    v.as_object().cloned().unwrap_or_default()
}

fn unhex(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16).unwrap_or(0);
        let lo = (pair[1] as char).to_digit(16).unwrap_or(0);
        out.push((hi * 16 + lo) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// Screen: vte Perform -> grid of styled cells
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Cell {
    sym: String, // "" marks the tail of a wide char (skipped when rendering)
    style: Style,
}

impl Cell {
    fn blank() -> Self {
        Self {
            sym: " ".into(),
            style: Style::new(),
        }
    }
}

/// One pane's terminal state: main + alt screens, scroll region,
/// cursor, SGR pen, and DSR replies waiting to be sent back.
struct Screen {
    cells: Vec<Cell>,
    alt: Vec<Cell>,
    on_alt: bool,
    rows: usize,
    cols: usize,
    cur_row: usize,
    cur_col: usize,
    saved_cursor: Option<(usize, usize, Style, bool)>,
    scroll_top: usize,
    scroll_bot: usize,
    pen: Style,
    replies: Vec<u8>,
    title: String,
    cursor_visible: bool,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            cells: vec![Cell::blank(); rows * cols],
            alt: vec![Cell::blank(); rows * cols],
            on_alt: false,
            rows,
            cols,
            cur_row: 0,
            cur_col: 0,
            saved_cursor: None,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            pen: Style::new(),
            replies: Vec::new(),
            title: String::new(),
            cursor_visible: true,
        }
    }

    fn buf(&mut self) -> &mut Vec<Cell> {
        if self.on_alt {
            &mut self.alt
        } else {
            &mut self.cells
        }
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        let rebuild = |old: &[Cell], orows: usize, ocols: usize| -> Vec<Cell> {
            let mut v = vec![Cell::blank(); rows * cols];
            for r in 0..rows.min(orows) {
                for c in 0..cols.min(ocols) {
                    v[r * cols + c] = old[r * ocols + c].clone();
                }
            }
            v
        };
        self.cells = rebuild(&self.cells, self.rows, self.cols);
        self.alt = rebuild(&self.alt, self.rows, self.cols);
        self.rows = rows;
        self.cols = cols;
        self.cur_row = self.cur_row.min(rows.saturating_sub(1));
        self.cur_col = self.cur_col.min(cols.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bot = rows.saturating_sub(1);
    }

    fn clear_all(&mut self) {
        for c in self.buf().iter_mut() {
            *c = Cell::blank();
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let (top, bot) = (self.scroll_top, self.scroll_bot);
        let width = self.cols;
        for _ in 0..n {
            let buf = self.buf();
            buf.drain(top * width..(top + 1) * width);
            for _ in 0..width {
                buf.insert(bot * width, Cell::blank());
            }
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let (top, bot) = (self.scroll_top, self.scroll_bot);
        let width = self.cols;
        for _ in 0..n {
            let buf = self.buf();
            buf.drain(bot * width..(bot + 1) * width);
            for _ in 0..width {
                buf.insert(top * width, Cell::blank());
            }
        }
    }

    fn linefeed(&mut self) {
        if self.cur_row == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cur_row < self.rows - 1 {
            self.cur_row += 1;
        }
    }

    fn put(&mut self, ch: char) {
        let w = ch.width().unwrap_or(1).max(1);
        if self.cur_col >= self.cols {
            self.cur_col = 0;
            self.linefeed();
        }
        let (r, c) = (self.cur_row, self.cur_col);
        let idx = r * self.cols + c;
        // overwrite: clear a wide-char head or tail at this position so a
        // shorter repaint never leaves half of the previous glyph behind
        if self.buf()[idx].sym.is_empty() && idx > r * self.cols {
            self.buf()[idx - 1] = Cell::blank();
        }
        self.buf()[idx] = Cell {
            sym: ch.to_string(),
            style: self.pen,
        };
        if w == 2 && c + 1 < self.cols {
            self.buf()[idx + 1] = Cell {
                sym: String::new(),
                style: self.pen,
            };
            self.cur_col += 1;
        }
        self.cur_col += 1;
    }

    fn erase(&mut self, from: usize, to: usize) {
        for i in from..to.min(self.buf().len()) {
            self.buf()[i] = Cell::blank();
        }
    }

    fn drain_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }
}

fn color_from_sgr(code: u16, bright: bool) -> Option<Color> {
    const BASIC: [Color; 8] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
    ];
    const BRIGHT: [Color; 8] = [
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    if code == 8 || code == 9 {
        return None;
    }
    let idx = (code - 30) % 10;
    if code >= 30 && code <= 37 {
        return Some(BASIC[idx as usize]);
    }
    if code >= 90 && code <= 97 {
        return Some(BRIGHT[idx as usize]);
    }
    if bright {
        return Some(BRIGHT[idx as usize]);
    }
    Some(BASIC[idx as usize])
}

impl vte::Perform for Screen {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cur_col = 0,
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            b'\x08' => self.cur_col = self.cur_col.saturating_sub(1),
            b'\t' => {
                let next = (self.cur_col / 8 + 1) * 8;
                self.cur_col = next.min(self.cols.saturating_sub(1));
            }
            0x07 => {} // BEL: ignore (no bell UI)
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ig: bool,
        action: char,
    ) {
        let p0 = params
            .iter()
            .next()
            .and_then(|p| p.first().copied())
            .unwrap_or(0);
        let n = p0.max(1) as usize;
        let priv_mode = intermediates.contains(&b'?');
        match action {
            'A' => self.cur_row = self.cur_row.saturating_sub(n),
            'B' => self.cur_row = (self.cur_row + n).min(self.rows - 1),
            'C' => self.cur_col = (self.cur_col + n).min(self.cols - 1),
            'D' => self.cur_col = self.cur_col.saturating_sub(n),
            'E' => {
                self.cur_row = (self.cur_row + n).min(self.rows - 1);
                self.cur_col = 0;
            }
            'F' => {
                self.cur_row = self.cur_row.saturating_sub(n);
                self.cur_col = 0;
            }
            'G' | '`' => self.cur_col = (n - 1).min(self.cols - 1),
            'd' => self.cur_row = (n - 1).min(self.rows - 1),
            'H' | 'f' => {
                let row = p0.max(1) as usize;
                let col = params
                    .iter()
                    .nth(1)
                    .and_then(|p| p.first().copied())
                    .unwrap_or(1)
                    .max(1) as usize;
                self.cur_row = (row - 1).min(self.rows - 1);
                self.cur_col = (col - 1).min(self.cols - 1);
            }
            'J' => {
                let idx = self.cur_row * self.cols + self.cur_col;
                let len = self.buf().len();
                match p0 {
                    0 => self.erase(idx, len),
                    1 => self.erase(0, idx + 1),
                    _ => self.erase(0, len),
                }
            }
            'K' => {
                let start = self.cur_row * self.cols;
                let idx = start + self.cur_col;
                let end = start + self.cols;
                match p0 {
                    0 => self.erase(idx, end),
                    1 => self.erase(start, idx + 1),
                    _ => self.erase(start, end),
                }
            }
            'L' => {
                // insert n blank lines at cur_row (within scroll region)
                let (top, bot) = (self.cur_row, self.scroll_bot);
                let w = self.cols;
                if top <= bot {
                    let shift = n.min(bot - top + 1);
                    for r in ((top + shift)..=bot).rev() {
                        let (dst, src) = (r * w, (r - shift) * w);
                        let row: Vec<Cell> = self.buf()[src..src + w].to_vec();
                        self.buf()[dst..dst + w].clone_from_slice(&row);
                    }
                    for r in top..top + shift {
                        let at = r * w;
                        self.erase(at, at + w);
                    }
                }
            }
            'M' => {
                // delete n lines at cur_row (within scroll region)
                let (top, bot) = (self.cur_row, self.scroll_bot);
                let w = self.cols;
                if top <= bot {
                    let shift = n.min(bot - top + 1);
                    for r in top..=bot - shift {
                        let (dst, src) = (r * w, (r + shift) * w);
                        let row: Vec<Cell> = self.buf()[src..src + w].to_vec();
                        self.buf()[dst..dst + w].clone_from_slice(&row);
                    }
                    for r in bot + 1 - shift..=bot {
                        let at = r * w;
                        self.erase(at, at + w);
                    }
                }
            }
            'P' => {
                // delete n chars at cursor, rest of line shifts left
                let w = self.cols;
                let line_end = (self.cur_row + 1) * w;
                let start = self.cur_row * w + self.cur_col;
                let take = n.min(w - self.cur_col);
                let tail: Vec<Cell> = self.buf()[start + take..line_end].to_vec();
                let buf = self.buf();
                for (i, c) in tail.into_iter().enumerate() {
                    buf[start + i] = c;
                }
                for i in line_end - take..line_end {
                    buf[i] = Cell::blank();
                }
            }
            '@' => {
                // insert n blanks at cursor, rest shifts right
                let w = self.cols;
                let line_end = (self.cur_row + 1) * w;
                let start = self.cur_row * w + self.cur_col;
                let take = n.min(w - self.cur_col);
                let head: Vec<Cell> = self.buf()[start..line_end - take].to_vec();
                let buf = self.buf();
                for (i, c) in head.into_iter().enumerate() {
                    buf[start + take + i] = c;
                }
                for i in start..start + take {
                    buf[i] = Cell::blank();
                }
            }
            'S' => self.scroll_up(n),
            'T' => self.scroll_down(n),
            'r' => {
                let top = p0.saturating_sub(1) as usize;
                let bot = params
                    .iter()
                    .nth(1)
                    .and_then(|p| p.first().copied())
                    .unwrap_or(self.rows as u16)
                    .saturating_sub(1) as usize;
                if top < bot && bot < self.rows {
                    self.scroll_top = top;
                    self.scroll_bot = bot;
                    self.cur_row = top;
                    self.cur_col = 0;
                }
            }
            's' => {
                self.saved_cursor = Some((self.cur_row, self.cur_col, self.pen, self.on_alt));
            }
            'u' => {
                if let Some((r, c, pen, alt)) = self.saved_cursor {
                    self.cur_row = r.min(self.rows - 1);
                    self.cur_col = c.min(self.cols - 1);
                    self.pen = pen;
                    if alt != self.on_alt {
                        self.on_alt = alt;
                    }
                }
            }
            'm' => self.apply_sgr(params),
            'n' => {
                // DSR: synthesized from OUR grid state (the whole point
                // of pane_attach — nobody else will answer)
                if p0 == 6 {
                    let r = self.cur_row + 1;
                    let c = self.cur_col + 1;
                    self.replies
                        .extend_from_slice(format!("\x1b[{r};{c}R").as_bytes());
                } else if p0 == 5 {
                    self.replies.extend_from_slice(b"\x1b[0n");
                }
            }
            'h' | 'l' => {
                if priv_mode {
                    for p in params.iter() {
                        let Some(code) = p.first() else { continue };
                        let code = if *code >= 10_000 {
                            code - 10_000
                        } else {
                            *code
                        };
                        let on = action == 'h';
                        match code {
                            1049 => {
                                self.on_alt = on;
                                if on {
                                    self.clear_all();
                                }
                                self.cur_row = 0;
                                self.cur_col = 0;
                            }
                            25 => self.cursor_visible = on,
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _i: &[u8], _ig: bool, byte: u8) {
        match byte as char {
            '7' => self.saved_cursor = Some((self.cur_row, self.cur_col, self.pen, self.on_alt)),
            '8' => {
                if let Some((r, c, pen, alt)) = self.saved_cursor {
                    self.cur_row = r.min(self.rows - 1);
                    self.cur_col = c.min(self.cols - 1);
                    self.pen = pen;
                    self.on_alt = alt;
                }
            }
            'D' => self.linefeed(),
            'M' => {
                if self.cur_row == self.scroll_top {
                    self.scroll_down(1);
                } else {
                    self.cur_row = self.cur_row.saturating_sub(1);
                }
            }
            'E' => {
                self.cur_col = 0;
                self.linefeed();
            }
            'c' => {
                self.clear_all();
                self.cur_row = 0;
                self.cur_col = 0;
                self.scroll_top = 0;
                self.scroll_bot = self.rows - 1;
                self.pen = Style::new();
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        // OSC 0/2: window title — shown in our status bar
        if params
            .first()
            .map(|p| p.is_empty() || *p == b"0" || *p == b"2")
            .unwrap_or(false)
        {
            if let Some(t) = params.get(1) {
                self.title = String::from_utf8_lossy(t)
                    .trim_end_matches('\x07')
                    .to_string();
            }
        }
    }
}

impl Screen {
    fn apply_sgr(&mut self, params: &vte::Params) {
        if params.is_empty() {
            self.pen = Style::new();
            return;
        }
        let flat: Vec<u16> = params.iter().flat_map(|p| p.to_vec()).collect();
        let mut i = 0;
        while i < flat.len() {
            match flat[i] {
                0 => self.pen = Style::new(),
                1 => self.pen = self.pen.add_modifier(Modifier::BOLD),
                3 => self.pen = self.pen.add_modifier(Modifier::ITALIC),
                4 => self.pen = self.pen.add_modifier(Modifier::UNDERLINED),
                7 => self.pen = self.pen.add_modifier(Modifier::REVERSED),
                22 => self.pen = self.pen.remove_modifier(Modifier::BOLD),
                23 => self.pen = self.pen.remove_modifier(Modifier::ITALIC),
                24 => self.pen = self.pen.remove_modifier(Modifier::UNDERLINED),
                27 => self.pen = self.pen.remove_modifier(Modifier::REVERSED),
                39 => self.pen.fg = None,
                49 => self.pen.bg = None,
                30..=37 => self.pen.fg = color_from_sgr(flat[i], false),
                40..=47 => {
                    self.pen.bg = color_from_sgr(flat[i] - 10, false);
                }
                90..=97 => self.pen.fg = color_from_sgr(flat[i], true),
                100..=107 => {
                    self.pen.bg = color_from_sgr(flat[i] - 10, true);
                }
                38 | 48 => {
                    // 38;5;N  or  38;2;R;G;B
                    let is_fg = flat[i] == 38;
                    if i + 1 < flat.len() && flat[i + 1] == 5 {
                        if let Some(n) = flat.get(i + 2) {
                            let col = ansi256(*n);
                            if is_fg {
                                self.pen.fg = Some(col)
                            } else {
                                self.pen.bg = Some(col)
                            }
                        }
                        i += 2;
                    } else if i + 1 < flat.len() && flat[i + 1] == 2 {
                        if let (Some(r), Some(g), Some(b)) =
                            (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4))
                        {
                            let col = Color::Rgb(*r as u8, *g as u8, *b as u8);
                            if is_fg {
                                self.pen.fg = Some(col)
                            } else {
                                self.pen.bg = Some(col)
                            }
                        }
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Plain-text projection of the grid (self-test + logging).
    fn plain_text(&self) -> String {
        let buf = if self.on_alt { &self.alt } else { &self.cells };
        let mut out = String::new();
        for r in 0..self.rows {
            let mut line = String::new();
            for c in 0..self.cols {
                let sym = &buf[r * self.cols + c].sym;
                if !sym.is_empty() {
                    line.push_str(sym);
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

fn ansi256(n: u16) -> Color {
    match n {
        0..=15 => {
            // 0-7 basic, 8-15 bright — map onto ratatui named colors
            const M: [Color; 16] = [
                Color::Black,
                Color::Red,
                Color::Green,
                Color::Yellow,
                Color::Blue,
                Color::Magenta,
                Color::Cyan,
                Color::Gray,
                Color::DarkGray,
                Color::LightRed,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightBlue,
                Color::LightMagenta,
                Color::LightCyan,
                Color::White,
            ];
            M[n as usize]
        }
        232..=255 => {
            let v = 8 + (n - 232) as u8 * 10 / 24;
            Color::Rgb(v, v, v)
        }
        _ => {
            let n = n - 16;
            let c = |x: u16| -> u8 {
                let step = x / 36;
                if step == 0 {
                    0
                } else {
                    (step * 40 + 55) as u8
                }
            };
            Color::Rgb(c(n / 36), c((n % 36) / 6), c(n % 6))
        }
    }
}

// ---------------------------------------------------------------------------
// Pane driver: remote pane -> local screen
// ---------------------------------------------------------------------------

struct Pane {
    id: u32,
    cursor: u64,
    parser: vte::Parser,
    screen: Screen,
    alive: bool,
    last_output: Instant,
    attached: bool,
}

impl Pane {
    fn new(id: u32, rows: usize, cols: usize) -> Self {
        Self {
            id,
            cursor: 0,
            parser: vte::Parser::new(),
            screen: Screen::new(rows, cols),
            alive: true,
            last_output: Instant::now(),
            attached: false,
        }
    }

    /// Feed one pane_read payload; returns reply bytes for pane_send.
    fn pump(&mut self, hex: &str, alive: bool) -> Vec<u8> {
        let bytes = unhex(hex);
        if !bytes.is_empty() {
            for b in bytes {
                self.parser.advance(&mut self.screen, b);
            }
            self.last_output = Instant::now();
        }
        self.alive = alive;
        self.screen.drain_replies()
    }
}

// ---------------------------------------------------------------------------
// key -> ANSI encoding (crossterm events, not raw stdin, since ratatui
// owns the terminal)
// ---------------------------------------------------------------------------

fn key_to_ansi(
    code: crossterm::event::KeyCode,
    mods: crossterm::event::KeyModifiers,
) -> Option<String> {
    use crossterm::event::{KeyCode as K, KeyModifiers as M};
    let ctrl = mods.contains(M::CONTROL);
    let shift = mods.contains(M::SHIFT);
    match code {
        K::Char(c) => {
            if ctrl && !mods.intersects(M::ALT) {
                let up = c.to_ascii_uppercase();
                if up.is_ascii_uppercase() {
                    return Some(((up as u8 - b'A' + 1) as char).to_string());
                }
                match up {
                    '@' => return Some("\x00".into()),
                    '[' => return Some("\x1b".into()),
                    '\\' => return Some("\x1c".into()),
                    ']' => return Some("\x1d".into()),
                    '^' => return Some("\x1e".into()),
                    '_' => return Some("\x1f".into()),
                    ' ' => return Some("\x00".into()),
                    _ => return None,
                }
            }
            if mods.contains(M::ALT) {
                return Some(format!("\x1b{c}"));
            }
            if shift {
                return Some(c.to_uppercase().to_string());
            }
            Some(c.to_string())
        }
        K::Enter => Some("\r".into()),
        K::Backspace => Some(if mods.contains(M::ALT) {
            "\x1b\x7f".into()
        } else {
            "\x7f".into()
        }),
        K::Tab => Some(if shift { "\x1b[Z".into() } else { "\t".into() }),
        K::Esc => Some("\x1b".into()),
        K::Up => Some(if mods.contains(M::ALT) {
            "\x1b\x1b[A".into()
        } else {
            "\x1b[A".into()
        }),
        K::Down => Some("\x1b[B".into()),
        K::Right => Some("\x1b[C".into()),
        K::Left => Some("\x1b[D".into()),
        K::Home => Some("\x1b[H".into()),
        K::End => Some("\x1b[F".into()),
        K::PageUp => Some("\x1b[5~".into()),
        K::PageDown => Some("\x1b[6~".into()),
        K::Delete => Some("\x1b[3~".into()),
        K::Insert => Some("\x1b[2~".into()),
        K::F(n) => match n {
            1 => Some("\x1bOP".into()),
            2 => Some("\x1bOQ".into()),
            3 => Some("\x1bOR".into()),
            4 => Some("\x1bOS".into()),
            5 => Some("\x1b[15~".into()),
            6 => Some("\x1b[17~".into()),
            7 => Some("\x1b[18~".into()),
            8 => Some("\x1b[19~".into()),
            9 => Some("\x1b[20~".into()),
            10 => Some("\x1b[21~".into()),
            11 => Some("\x1b[23~".into()),
            12 => Some("\x1b[24~".into()),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, panes: &[Pane], focus: usize, url: &str) {
    let area = f.area();
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let main = chunks[0];
    let bar = chunks[1];

    if let Some(p) = panes.get(focus) {
        render_grid(f, main, p);
    } else {
        f.render_widget(Paragraph::new("no panes — Ctrl-B c to open"), main);
    }
    f.render_widget(status_line(panes, focus, url), bar);
}

fn render_grid(f: &mut Frame, area: Rect, p: &Pane) {
    let scr = &p.screen;
    let buf = if scr.on_alt { &scr.alt } else { &scr.cells };
    let content = f.area(); // silence unused warnings in some versions
    let _ = content;
    for r in 0..scr.rows.min(area.height as usize) {
        for c in 0..scr.cols.min(area.width as usize) {
            let cell = &buf[r * scr.cols + c];
            if cell.sym.is_empty() {
                continue; // wide-char tail
            }
            let x = area.x + c as u16;
            let y = area.y + r as u16;
            if let Some(ui) = f.buffer_mut().cell_mut((x, y)) {
                ui.set_symbol(&cell.sym).set_style(cell.style);
            }
        }
    }
}

fn status_line(panes: &[Pane], focus: usize, url: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " vmi-tui ".to_string(),
        Style::new().fg(Color::Black).bg(Color::Cyan),
    )];
    if panes.is_empty() {
        spans.push(Span::raw("no panes"));
    }
    for (i, p) in panes.iter().enumerate() {
        let mark = if i == focus { '*' } else { ' ' };
        let state = if !p.alive { "dead" } else { "live" };
        let idle = p.last_output.elapsed().as_secs();
        let txt = format!(" {mark}{}[{state},{idle}s] ", p.id);
        let style = if i == focus {
            Style::new().fg(Color::Black).bg(Color::Green)
        } else {
            Style::new().fg(Color::Gray)
        };
        spans.push(Span::styled(txt, style));
    }
    let title = panes
        .get(focus)
        .map(|p| p.screen.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_default();
    let help = " Ctrl-B: n/p next/prev, c open, x kill, 0-9 jump, q quit ";
    spans.push(Span::raw(format!(" {title} | {url} ")));
    spans.push(Span::styled(
        help.to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

struct Args {
    url: String,
    pane: Option<u32>,
    self_test: bool,
}

fn parse_args() -> Result<Args> {
    let mut url = None;
    let mut pane = None;
    let mut self_test = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => url = Some(it.next().context("--url needs a value")?),
            "--pane" => pane = Some(it.next().context("--pane needs an id")?.parse()?),
            "--self-test" => self_test = true,
            _ => bail!("unknown arg {a}; usage: vmi-tui [--url http://ip:9881/mcp] [--pane N] [--self-test]"),
        }
    }
    Ok(Args {
        url: url.unwrap_or_else(|| "http://127.0.0.1:9881/mcp".into()),
        pane,
        self_test,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::default(),
        StreamableHttpClientTransportConfig::with_uri(args.url.clone()),
    );
    let svc = ClientConfig::default()
        .serve(transport)
        .await
        .with_context(|| format!("connect {}", args.url))?;

    let (rows, cols) = crossterm::terminal::size()
        .map(|(c, r)| (r as usize, c as usize))
        .unwrap_or((24, 80));
    let rows = rows.max(2);
    let cols = cols.max(8);

    // discover panes; open one if the workbench is empty
    let listed = call(&svc, "pane_list", obj(json!({})))
        .await
        .context("pane_list")?;
    let mut ids: Vec<u32> = listed
        .get("panes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get("id").and_then(Value::as_u64).map(|i| i as u32))
                .collect()
        })
        .unwrap_or_default();
    ids.sort_unstable();
    if ids.is_empty() {
        let opened = call(&svc, "pane_open", obj(json!({})))
            .await
            .context("pane_open")?;
        ids.push(opened.get("id").and_then(Value::as_u64).unwrap_or(0) as u32);
    }

    let mut panes: Vec<Pane> = ids.iter().map(|&id| Pane::new(id, rows, cols)).collect();
    let mut focus = args
        .pane
        .and_then(|want| panes.iter().position(|p| p.id == want))
        .unwrap_or(0);

    if args.self_test {
        return self_test_run(&svc, &mut panes, focus).await;
    }

    // TUI owns the terminal: alt screen + raw mode, restored on drop
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    crossterm::terminal::enable_raw_mode()?;
    let tui_result = tui_run(&svc, &mut panes, &mut focus, &args.url, cols, rows).await;
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen)?;
    tui_result
}

async fn tui_run(
    svc: &Client,
    panes: &mut Vec<Pane>,
    focus: &mut usize,
    url: &str,
    cols: usize,
    rows: usize,
) -> Result<()> {
    let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut events = crossterm::event::EventStream::new();
    let mut pending_prefix = false;
    let mut need_resize = (rows, cols);

    // focus takes DSR ownership
    if let Some(p) = panes.get(*focus) {
        let _ = call(svc, "pane_attach", obj(json!({ "id": p.id }))).await;
        panes[*focus].attached = true;
    }
    // match the remote pty to our size
    if let Some(p) = panes.get(*focus) {
        let _ = call(
            svc,
            "pane_resize",
            obj(json!({ "id": p.id, "cols": need_resize.1, "rows": need_resize.0 })),
        )
        .await;
    }

    loop {
        // ---- poll panes ----
        for i in 0..panes.len() {
            let id = panes[i].id;
            let cursor = panes[i].cursor;
            let v = call(
                svc,
                "pane_read",
                obj(json!({ "id": id, "cursor": cursor, "max_bytes": 65536 })),
            )
            .await;
            match v {
                Ok(v) => {
                    let hex = v
                        .get("data_hex")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let new_cursor = v.get("cursor").and_then(Value::as_u64).unwrap_or(cursor);
                    let alive = v.get("alive").and_then(Value::as_bool).unwrap_or(true);
                    let replies = panes[i].pump(&hex, alive);
                    panes[i].cursor = new_cursor;
                    if !replies.is_empty() {
                        let text = String::from_utf8_lossy(&replies).to_string();
                        let _ =
                            call(svc, "pane_send", obj(json!({ "id": id, "text": text }))).await;
                    }
                }
                Err(_) => panes[i].alive = false,
            }
        }

        // ---- draw ----
        terminal.draw(|f| draw(f, panes, *focus, url))?;

        // ---- input / tick ----
        let ev = tokio::select! {
            e = events.next() => e,
            _ = tokio::time::sleep(Duration::from_millis(50)) => None,
        };
        let Some(Ok(event)) = ev else {
            continue;
        };
        match event {
            crossterm::event::Event::Resize(w, h) => {
                need_resize = (h as usize, w as usize);
                for p in panes.iter_mut() {
                    p.screen.resize(need_resize.0.max(2), need_resize.1.max(8));
                }
                if let Some(p) = panes.get(*focus) {
                    let _ = call(
                        svc,
                        "pane_resize",
                        obj(json!({ "id": p.id, "cols": need_resize.1, "rows": need_resize.0 })),
                    )
                    .await;
                }
            }
            crossterm::event::Event::Key(k) => {
                if !matches!(k.kind, crossterm::event::KeyEventKind::Press) {
                    continue;
                }
                let ctrl = k
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL);
                if pending_prefix {
                    pending_prefix = false;
                    match k.code {
                        crossterm::event::KeyCode::Char('q') => break,
                        crossterm::event::KeyCode::Char('n') => {
                            let len = panes.len().max(1);
                            let next = (*focus + 1) % len;
                            switch_focus(svc, panes, focus, next).await;
                        }
                        crossterm::event::KeyCode::Char('p') => {
                            let len = panes.len().max(1);
                            let prev = (*focus + len - 1) % len;
                            switch_focus(svc, panes, focus, prev).await;
                        }
                        crossterm::event::KeyCode::Char('c') => {
                            if let Ok(v) = call(svc, "pane_open", obj(json!({}))).await {
                                let id = v.get("id").and_then(Value::as_u64).unwrap_or(0) as u32;
                                let (r, c) = (need_resize.0, need_resize.1);
                                let _ = call(
                                    svc,
                                    "pane_resize",
                                    obj(json!({ "id": id, "cols": c, "rows": r })),
                                )
                                .await;
                                let mut np = Pane::new(id, r.max(2), c.max(8));
                                np.attached = true;
                                let _ = call(svc, "pane_attach", obj(json!({ "id": id }))).await;
                                panes.push(np);
                                *focus = panes.len() - 1;
                            }
                        }
                        crossterm::event::KeyCode::Char('x') => {
                            if let Some(p) = panes.get(*focus) {
                                let _ = call(svc, "pane_kill", obj(json!({ "id": p.id }))).await;
                                let dead_id = p.id;
                                panes.retain(|p| p.id != dead_id);
                                if *focus >= panes.len() {
                                    *focus = panes.len().saturating_sub(1);
                                }
                            }
                        }
                        crossterm::event::KeyCode::Char(d @ '0'..='9') => {
                            let idx = (d as u8 - b'0') as usize;
                            if idx < panes.len() {
                                switch_focus(svc, panes, focus, idx).await;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                if ctrl && k.code == crossterm::event::KeyCode::Char('b') {
                    pending_prefix = true;
                    continue;
                }
                // forward to focused pane
                if let Some(text) = key_to_ansi(k.code, k.modifiers) {
                    if let Some(p) = panes.get(*focus) {
                        let _ =
                            call(svc, "pane_send", obj(json!({ "id": p.id, "text": text }))).await;
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(p) = panes.get(*focus) {
        let _ = call(svc, "pane_detach", obj(json!({ "id": p.id }))).await;
    }
    Ok(())
}

async fn switch_focus(svc: &Client, panes: &mut [Pane], focus: &mut usize, new: usize) {
    if new == *focus || new >= panes.len() {
        return;
    }
    let old = panes[*focus].id;
    let _ = call(svc, "pane_detach", obj(json!({ "id": old }))).await;
    panes[*focus].attached = false;
    *focus = new;
    let id = panes[new].id;
    let _ = call(svc, "pane_attach", obj(json!({ "id": id }))).await;
    panes[*focus].attached = true;
    let _ = call(
        svc,
        "pane_resize",
        obj(json!({
            "id": id,
            "cols": panes[*focus].screen.cols,
            "rows": panes[*focus].screen.rows
        })),
    )
    .await;
}

/// Headless verification: drive one pane, render the grid as text.
async fn self_test_run(svc: &Client, panes: &mut Vec<Pane>, focus: usize) -> Result<()> {
    let id = panes[focus].id;
    let marker = "tui-selftest-ok";
    let _ = call(
        svc,
        "pane_send",
        obj(json!({ "id": id, "text": format!("echo {marker}\r") })),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let v = call(
            svc,
            "pane_read",
            obj(json!({ "id": id, "cursor": panes[focus].cursor, "max_bytes": 65536 })),
        )
        .await?;
        let hex = v
            .get("data_hex")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let cursor = v
            .get("cursor")
            .and_then(Value::as_u64)
            .unwrap_or(panes[focus].cursor);
        let alive = v.get("alive").and_then(Value::as_bool).unwrap_or(true);
        let replies = panes[focus].pump(&hex, alive);
        panes[focus].cursor = cursor;
        if !replies.is_empty() {
            let text = String::from_utf8_lossy(&replies).to_string();
            let _ = call(svc, "pane_send", obj(json!({ "id": id, "text": text }))).await;
        }
        let text = panes[focus].screen.plain_text();
        if text.contains(marker) {
            let mut out = std::io::stdout();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            eprintln!("[vmi-tui self-test] OK: marker rendered");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    bail!(
        "self-test: marker never appeared; grid so far:\n{}",
        panes[focus].screen.plain_text()
    )
}

// ---------------------------------------------------------------------------
// tests: VT semantics land in the grid correctly
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(scr: &mut Screen, s: &str) -> Vec<u8> {
        let mut parser = vte::Parser::new();
        for b in s.bytes() {
            parser.advance(scr, b);
        }
        scr.drain_replies()
    }

    #[test]
    fn plain_lines() {
        let mut s = Screen::new(4, 20);
        feed(&mut s, "abc\r\ndef");
        assert_eq!(s.plain_text().lines().next(), Some("abc"));
        assert!(s.plain_text().contains("def"));
    }

    #[test]
    fn cup_and_erase() {
        let mut s = Screen::new(4, 20);
        feed(&mut s, "hello");
        feed(&mut s, "\x1b[2;1H");
        assert_eq!((s.cur_row, s.cur_col), (1, 0));
        feed(&mut s, "\x1b[2J");
        assert_eq!(s.plain_text().trim(), "");
    }

    #[test]
    fn wrap_at_eol() {
        let mut s = Screen::new(4, 5);
        feed(&mut s, "abcdefg");
        let t = s.plain_text();
        assert!(t.lines().next().unwrap().starts_with("abcde"));
        assert!(t.contains("fg"));
    }

    #[test]
    fn dsr_replied_from_grid() {
        let mut s = Screen::new(4, 20);
        feed(&mut s, "ab");
        let replies = feed(&mut s, "\x1b[6n");
        assert_eq!(replies, b"\x1b[1;3R");
    }

    #[test]
    fn sgr_colors_parse() {
        let mut s = Screen::new(2, 10);
        feed(&mut s, "\x1b[1;31mx");
        assert_eq!(s.cells[0].sym, "x");
        assert_eq!(s.cells[0].style.fg, Some(Color::Red));
        assert!(s.cells[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn alt_screen_roundtrip() {
        let mut s = Screen::new(4, 20);
        feed(&mut s, "main");
        feed(&mut s, "\x1b[?1049h");
        assert!(s.on_alt);
        assert_eq!(s.plain_text().trim(), "");
        feed(&mut s, "alt!");
        feed(&mut s, "\x1b[?1049l");
        assert!(!s.on_alt);
        assert!(s.plain_text().contains("main"));
    }

    #[test]
    fn scroll_region() {
        let mut s = Screen::new(4, 10);
        for i in 0..8 {
            feed(&mut s, &format!("l{i}\r\n"));
        }
        let t = s.plain_text();
        assert!(t.contains("l7"));
    }

    #[test]
    fn wide_char_takes_two_cells() {
        let mut s = Screen::new(2, 10);
        feed(&mut s, "中");
        assert_eq!(s.cells[0].sym, "中");
        assert_eq!(s.cells[1].sym, "");
        assert_eq!(s.cur_col, 2);
    }
}
