//! claude-hl: run a TUI (default: claude) inside a PTY and paint shell
//! commands in its output, Codex-style.
//!
//!   claude-hl [args passed to claude...]
//!   CLAUDE_HL_CMD=codex claude-hl        # wrap something else
//!   CLAUDE_HL_THEME=rose claude-hl       # rose-pine palette (default: codex)
//!   CLAUDE_HL_DUMP=/path claude-hl       # also append the raw PTY stream to a file (debug)
//!   claude-hl --selftest                 # print sample highlighted text
//!
//! How it works: the child's raw output is passed through untouched while a
//! small VT emulator mirrors the screen (cursor, cell grid, attributes).
//! After each chunk, rows whose text changed are re-tokenised and the cells
//! whose colour should differ from what is on screen are repainted with
//! absolute cursor moves; the cursor and attributes are then restored.
//! This survives renderers that stream a line in pieces (Claude Code does).
//! Width is never changed, so the TUI layout survives.

use std::collections::HashSet;
use std::io::Write;
use std::rc::Rc;
use std::sync::OnceLock;

// ---- palette ---------------------------------------------------------------

struct Theme { cmd: &'static str, sub: &'static str, flag: &'static str, string: &'static str, path: &'static str, op: &'static str }

const THEME_ROSE: Theme = Theme { cmd: "9ccfd8", sub: "c4a7e7", flag: "ebbcba", string: "f6c177", path: "e0def4", op: "9ccfd8" };
const THEME_CODEX: Theme = Theme { cmd: "6fb3ff", sub: "e6e6e6", flag: "e78fc7", string: "e5c07b", path: "d0d0d0", op: "6fb3ff" };

/// Colour classes; index 0 = "no override".
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Color { None = 0, Cmd, Sub, Flag, Str, Path, Op }

fn rgb(h: &str) -> String {
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(255);
    format!("\x1b[38;2;{};{};{}m", c(0), c(2), c(4))
}

/// SGR strings indexed by Color.
fn palette() -> &'static [String; 7] {
    static P: OnceLock<[String; 7]> = OnceLock::new();
    P.get_or_init(|| {
        let t = match std::env::var("CLAUDE_HL_THEME").as_deref() {
            Ok("rose") => THEME_ROSE,
            _ => THEME_CODEX,
        };
        [String::new(), rgb(t.cmd), rgb(t.sub), rgb(t.flag), rgb(t.string), rgb(t.path), rgb(t.op)]
    })
}

// ---- vocabulary ------------------------------------------------------------

const COMMANDS: &str = "git gh npm npx pnpm yarn bun bunx node deno python python3 pip pip3 uv go cargo \
rustc make cmake brew apt apt-get dnf pacman docker docker-compose kubectl helm \
terraform aws gcloud az ssh scp rsync curl wget tar zip unzip cd ls cat head \
tail less grep rg fd find xargs sed awk sort uniq wc tr cut tee echo printf \
export source chmod chown mkdir rmdir rm cp mv ln touch pwd which env nvim vim \
tmux ghostty claude codex gemini open kill pkill ps lsof jq yq tsc tsx vitest \
jest eslint prettier next vite";

/// tools whose bare-word args are subcommands; for others (node, cat, cd...)
/// only flags/paths/strings count, so prose like "node here" stays plain
const SUBCMD_TOOLS: &str = "git gh npm npx pnpm yarn bun bunx deno uv pip pip3 go cargo brew apt apt-get \
dnf pacman docker docker-compose kubectl helm terraform aws gcloud az claude \
codex gemini tmux make jq tsc next vite";

const STOP_WORDS: &str = "and or then to the a an in on for with is it that this if of at by from so but \
you we i will can should after before when";

/// bare words accepted after the command (e.g. `push origin main`)
const MAX_SUB: usize = 3;

struct Vocab { commands: HashSet<&'static str>, subcmd_tools: HashSet<&'static str>, stop_words: HashSet<&'static str> }

fn vocab() -> &'static Vocab {
    static V: OnceLock<Vocab> = OnceLock::new();
    V.get_or_init(|| Vocab {
        commands: COMMANDS.split_whitespace().collect(),
        subcmd_tools: SUBCMD_TOOLS.split_whitespace().collect(),
        stop_words: STOP_WORDS.split_whitespace().collect(),
    })
}

// ---- tokenizer -------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Kind { Ws, Str, Op, Flag, Path, Sub, Num }

fn is_ws(b: u8) -> bool { b == b' ' || b == b'\t' }
fn is_space(b: u8) -> bool { matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) }
fn is_word(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'_' }
/// chars that may not precede a command word
fn is_cmd_glue(b: u8) -> bool { is_word(b) || matches!(b, b'/' | b'.' | b'@' | b'~' | b'-') }
fn is_path_mark(b: u8) -> bool { matches!(b, b'/' | b'.' | b'~' | b'=' | b':' | b'@' | b'$') }
fn at_boundary(t: &[u8], i: usize) -> bool { i >= t.len() || is_space(t[i]) }

/// Match one argument token at `i`. Returns (kind, end).
fn next_arg(t: &[u8], i: usize) -> Option<(Kind, usize)> {
    let n = t.len();
    if i >= n { return None; }
    let b = t[i];
    if is_ws(b) {
        let mut j = i;
        while j < n && is_ws(t[j]) { j += 1; }
        return Some((Kind::Ws, j));
    }
    if b == b'"' {
        let mut j = i + 1;
        while j < n {
            match t[j] {
                b'\\' if j + 1 < n => j += 2,
                b'"' => return Some((Kind::Str, j + 1)),
                _ => j += 1,
            }
        }
    } else if b == b'\'' {
        if let Some(k) = t[i + 1..].iter().position(|&c| c == b'\'') {
            return Some((Kind::Str, i + 1 + k + 1));
        }
    }
    for op in [&b"&&"[..], b"||", b"|", b";", b"2>&1", b">>", b">", b"<"] {
        if t[i..].starts_with(op) && at_boundary(t, i + op.len()) {
            return Some((Kind::Op, i + op.len()));
        }
    }
    if b == b'-' {
        let mut j = i + 1;
        if j < n && t[j] == b'-' { j += 1; }
        if j < n && t[j].is_ascii_alphabetic() {
            j += 1;
            while j < n && (is_word(t[j]) || t[j] == b'-') { j += 1; }
            if j < n && t[j] == b'=' {
                j += 1;
                while j < n && !is_space(t[j]) { j += 1; }
            }
            return Some((Kind::Flag, j));
        }
        if j == i + 2 && at_boundary(t, j) { return Some((Kind::Flag, j)); }
    }
    {
        let mut j = i;
        let mut marked = false;
        while j < n && !is_space(t[j]) && t[j] != b'`' {
            if is_path_mark(t[j]) { marked = true; }
            j += 1;
        }
        if marked { return Some((Kind::Path, j)); }
    }
    if b.is_ascii_lowercase() {
        let mut j = i + 1;
        while j < n && (t[j].is_ascii_lowercase() || t[j].is_ascii_digit() || t[j] == b'-') { j += 1; }
        return Some((Kind::Sub, j));
    }
    if b.is_ascii_digit() {
        let mut j = i + 1;
        while j < n && t[j].is_ascii_digit() { j += 1; }
        return Some((Kind::Num, j));
    }
    None
}

/// Find the next known command word at or after `from`.
fn find_cmd(t: &[u8], from: usize) -> Option<(usize, usize)> {
    let v = vocab();
    let n = t.len();
    let mut i = from;
    while i < n {
        if is_space(t[i]) { i += 1; continue; }
        let start = i;
        while i < n && !is_space(t[i]) { i += 1; }
        let mut p = start;
        while p < i {
            if p == 0 || !is_cmd_glue(t[p - 1]) {
                if let Ok(w) = std::str::from_utf8(&t[p..i]) {
                    if v.commands.contains(w) { return Some((p, i)); }
                }
            }
            p += 1;
        }
    }
    None
}

/// Compute colour spans (byte ranges) for one line of text.
fn spans(text: &str, out: &mut Vec<(usize, usize, Color)>) {
    let t = text.as_bytes();
    let v = vocab();
    let mut pos = 0;
    while let Some((cs, ce)) = find_cmd(t, pos) {
        let mut cmd = &text[cs..ce];
        let mark = out.len();
        out.push((cs, ce, Color::Cmd));
        let mut i = ce;
        let mut subs = 0;
        let mut nargs = 0;
        let mut after_op = false;
        while let Some((kind, end)) = next_arg(t, i) {
            if kind == Kind::Ws { i = end; continue; }
            let tok = &text[i..end];
            if after_op && kind != Kind::Op {
                if kind == Kind::Sub && v.commands.contains(tok) {
                    out.push((i, end, Color::Cmd));
                    cmd = tok;
                    subs = 0;
                    after_op = false;
                    i = end;
                    continue;
                }
                break;
            }
            let color = match kind {
                Kind::Sub => {
                    if v.stop_words.contains(tok) || subs >= MAX_SUB || !v.subcmd_tools.contains(cmd) { break; }
                    subs += 1;
                    Color::Sub
                }
                Kind::Flag => Color::Flag,
                Kind::Str => Color::Str,
                Kind::Path | Kind::Num => Color::Path,
                Kind::Op => { after_op = true; Color::Op }
                Kind::Ws => unreachable!(),
            };
            // a bare word followed by sentence punctuation ends the span
            if matches!(kind, Kind::Sub | Kind::Path) && tok.len() > 1
                && matches!(t[end - 1], b'.' | b',' | b';' | b':' | b')')
            {
                out.push((i, end - 1, color));
                nargs += 1;
                i = end - 1;
                break;
            }
            out.push((i, end, color));
            nargs += 1;
            i = end;
        }
        if nargs == 0 {
            // bare command word in prose: leave it alone
            out.truncate(mark);
            pos = ce;
            continue;
        }
        pos = i;
    }
}

// ---- attributes ------------------------------------------------------------

#[derive(Clone, PartialEq, Default)]
struct Attr {
    fg: String, bg: String,
    bold: bool, dim: bool, italic: bool, underline: bool, blink: bool, inverse: bool, hidden: bool, strike: bool,
}

impl Attr {
    fn apply(&mut self, params: &[u16]) {
        let mut i = 0;
        let p = params;
        if p.is_empty() { *self = Attr::default(); return; }
        while i < p.len() {
            match p[i] {
                0 => *self = Attr::default(),
                1 => self.bold = true, 2 => self.dim = true, 3 => self.italic = true,
                4 => self.underline = true, 5 | 6 => self.blink = true, 7 => self.inverse = true,
                8 => self.hidden = true, 9 => self.strike = true,
                22 => { self.bold = false; self.dim = false }
                23 => self.italic = false, 24 => self.underline = false, 25 => self.blink = false,
                27 => self.inverse = false, 28 => self.hidden = false, 29 => self.strike = false,
                30..=37 | 90..=97 => self.fg = p[i].to_string(),
                40..=47 | 100..=107 => self.bg = p[i].to_string(),
                39 => self.fg.clear(), 49 => self.bg.clear(),
                38 | 48 => {
                    let n = if i + 1 < p.len() && p[i + 1] == 2 { 5 } else if i + 1 < p.len() && p[i + 1] == 5 { 3 } else { 1 };
                    let end = (i + n).min(p.len());
                    let s = p[i..end].iter().map(|x| x.to_string()).collect::<Vec<_>>().join(";");
                    if p[i] == 38 { self.fg = s } else { self.bg = s }
                    i = end - 1;
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// One SGR that reproduces this attribute set from scratch.
    fn render(&self, fg_override: &str) -> String {
        let mut s = String::from("\x1b[0");
        if self.bold { s.push_str(";1") } if self.dim { s.push_str(";2") } if self.italic { s.push_str(";3") }
        if self.underline { s.push_str(";4") } if self.blink { s.push_str(";5") } if self.inverse { s.push_str(";7") }
        if self.hidden { s.push_str(";8") } if self.strike { s.push_str(";9") }
        if !self.fg.is_empty() { s.push(';'); s.push_str(&self.fg) }
        if !self.bg.is_empty() { s.push(';'); s.push_str(&self.bg) }
        s.push('m');
        s.push_str(fg_override);
        s
    }
}

// ---- screen model ----------------------------------------------------------

#[derive(Clone)]
struct Cell { ch: char, zw: Option<Box<str>>, attr: Rc<Attr>, cont: bool, shown: Color }

fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x300 { return 1; }
    if matches!(u, 0x300..=0x36F | 0x200B..=0x200F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F) { return 0; }
    if matches!(u,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF |
        0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 |
        0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F680..=0x1F6FF | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD
    ) { return 2; }
    1
}

#[derive(Clone, Copy, PartialEq)]
enum PState { Ground, Esc, Csi, Osc, OscEsc, Str, StrEsc, EscInter }

struct Screen {
    rows: usize, cols: usize,
    grid: Vec<Vec<Cell>>,
    row: usize, col: usize,
    saved: (usize, usize, Rc<Attr>),
    top: usize, bottom: usize,          // scroll margins (inclusive)
    attr: Rc<Attr>,
    pending_wrap: bool,
    autowrap: bool,
    alt: bool,
    enabled: bool,
    dirty: Vec<bool>,
    // parser
    pst: PState,
    csi: Vec<u8>,
    utf8: Vec<u8>,
    // scratch
    spans_buf: Vec<(usize, usize, Color)>,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Self {
        let attr = Rc::new(Attr::default());
        let blank = Cell { ch: ' ', zw: None, attr: attr.clone(), cont: false, shown: Color::None };
        Screen {
            rows, cols,
            grid: vec![vec![blank; cols]; rows],
            row: 0, col: 0,
            saved: (0, 0, attr.clone()),
            top: 0, bottom: rows.saturating_sub(1),
            attr, pending_wrap: false, autowrap: true, alt: false, enabled: true,
            dirty: vec![false; rows],
            pst: PState::Ground, csi: Vec::new(), utf8: Vec::new(),
            spans_buf: Vec::new(),
        }
    }

    fn blank(&self) -> Cell { Cell { ch: ' ', zw: None, attr: self.attr.clone(), cont: false, shown: Color::None } }

    fn resize_grid(old: &[Vec<Cell>], blank: &Cell, rows: usize, cols: usize) -> Vec<Vec<Cell>> {
        let mut grid = vec![vec![blank.clone(); cols]; rows];
        for (r, row) in old.iter().enumerate().take(rows) {
            for (c, cell) in row.iter().enumerate().take(cols) { grid[r][c] = cell.clone(); }
        }
        grid
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        if rows == self.rows && cols == self.cols { return; }
        let blank = self.blank();
        self.grid = Self::resize_grid(&self.grid, &blank, rows, cols);
        self.rows = rows; self.cols = cols;
        self.row = self.row.min(rows.saturating_sub(1));
        self.col = self.col.min(cols.saturating_sub(1));
        self.top = 0; self.bottom = rows.saturating_sub(1);
        self.dirty = vec![true; rows];
        self.pending_wrap = false;
    }

    fn set_cursor(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows.saturating_sub(1));
        self.col = col.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            if self.top > self.bottom { break; }
            let blank = self.blank();
            let line = vec![blank; self.cols];
            self.grid.remove(self.top);
            self.grid.insert(self.bottom, line);
            self.dirty.remove(self.top);
            self.dirty.insert(self.bottom, false);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            if self.top > self.bottom { break; }
            let blank = self.blank();
            let line = vec![blank; self.cols];
            self.grid.remove(self.bottom);
            self.grid.insert(self.top, line);
            self.dirty.remove(self.bottom);
            self.dirty.insert(self.top, false);
        }
    }

    fn linefeed(&mut self) {
        if self.row == self.bottom { self.scroll_up(1); }
        else if self.row + 1 < self.rows { self.row += 1; }
    }

    fn put(&mut self, c: char) {
        let w = char_width(c);
        if w == 0 {
            // attach to the previous cell
            if self.col > 0 || self.pending_wrap {
                let col = if self.pending_wrap { self.col } else { self.col - 1 };
                let col = if self.grid[self.row][col].cont && col > 0 { col - 1 } else { col };
                let row = self.row;
                let cell = &mut self.grid[row][col];
                let mut s = cell.zw.as_deref().unwrap_or("").to_string();
                s.push(c);
                cell.zw = Some(s.into_boxed_str());
                cell.shown = Color::None;
                self.dirty[row] = true;
            }
            return;
        }
        if self.pending_wrap {
            self.pending_wrap = false;
            if self.autowrap { self.col = 0; self.linefeed(); }
        }
        if self.col + w > self.cols {
            if self.autowrap { self.col = 0; self.linefeed(); } else { self.col = self.cols - w; }
        }
        let row = self.row;
        let attr = self.attr.clone();
        self.grid[row][self.col] = Cell { ch: c, zw: None, attr: attr.clone(), cont: false, shown: Color::None };
        if w == 2 && self.col + 1 < self.cols {
            self.grid[row][self.col + 1] = Cell { ch: ' ', zw: None, attr, cont: true, shown: Color::None };
        }
        self.dirty[row] = true;
        if self.col + w >= self.cols { self.col = self.cols - 1; self.pending_wrap = true; }
        else { self.col += w; }
    }

    fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        let blank = self.blank();
        for c in from..to.min(self.cols) { self.grid[row][c] = blank.clone(); }
        self.dirty[row] = true;
    }

    fn erase_rows(&mut self, from: usize, to: usize) {
        for r in from..to.min(self.rows) { self.erase_cells(r, 0, self.cols); }
    }

    // -- byte feed -----------------------------------------------------------

    fn feed(&mut self, data: &[u8]) {
        for &b in data { self.feed_byte(b); }
    }

    fn feed_byte(&mut self, b: u8) {
        match self.pst {
            PState::Ground => {
                if b == 0x1b { self.utf8.clear(); self.pst = PState::Esc; return; }
                if b < 0x20 || b == 0x7f { self.utf8.clear(); self.control(b); return; }
                if b < 0x80 { self.utf8.clear(); self.put(b as char); return; }
                self.utf8.push(b);
                if let Ok(s) = std::str::from_utf8(&self.utf8) {
                    let c = s.chars().next().unwrap();
                    self.utf8.clear();
                    self.put(c);
                } else if self.utf8.len() >= 4 || (b & 0xC0) != 0x80 && self.utf8.len() > 1 {
                    self.utf8.clear();
                }
            }
            PState::Esc => {
                self.pst = PState::Ground;
                match b {
                    b'[' => { self.csi.clear(); self.pst = PState::Csi }
                    b']' => self.pst = PState::Osc,
                    b'P' | b'X' | b'^' | b'_' => self.pst = PState::Str,
                    b'(' | b')' | b'*' | b'+' | b'#' | b'%' => self.pst = PState::EscInter,
                    b'7' => self.saved = (self.row, self.col, self.attr.clone()),
                    b'8' => { let (r, c, a) = self.saved.clone(); self.set_cursor(r, c); self.attr = a; }
                    b'D' => self.linefeed(),
                    b'E' => { self.col = 0; self.linefeed(); }
                    b'M' => { if self.row == self.top { self.scroll_down(1); } else if self.row > 0 { self.row -= 1; } }
                    b'c' => { let (r, c, en) = (self.rows, self.cols, self.enabled); *self = Screen::new(r, c); self.enabled = en; self.dirty = vec![true; r]; }
                    _ => {}
                }
            }
            PState::EscInter => self.pst = PState::Ground,
            PState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.pst = PState::Ground;
                    let params = std::mem::take(&mut self.csi);
                    self.csi_dispatch(&params, b);
                } else if b == 0x1b { self.pst = PState::Esc; }
                else if b < 0x20 { self.control(b); }
                else { self.csi.push(b); }
            }
            PState::Osc => { if b == 0x07 { self.pst = PState::Ground } else if b == 0x1b { self.pst = PState::OscEsc } }
            PState::OscEsc => { self.pst = if b == b'\\' { PState::Ground } else { PState::Osc } }
            PState::Str => { if b == 0x1b { self.pst = PState::StrEsc } }
            PState::StrEsc => { self.pst = if b == b'\\' { PState::Ground } else { PState::Str } }
        }
    }

    fn control(&mut self, b: u8) {
        match b {
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            b'\r' => { self.col = 0; self.pending_wrap = false; }
            0x08 => { if self.col > 0 { self.col -= 1; } self.pending_wrap = false; }
            b'\t' => { self.col = ((self.col / 8 + 1) * 8).min(self.cols.saturating_sub(1)); self.pending_wrap = false; }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, raw: &[u8], fin: u8) {
        let private = raw.first().map_or(false, |&b| matches!(b, b'?' | b'>' | b'<' | b'=' | b'!'));
        let body = if private { &raw[1..] } else { raw };
        let mut params: Vec<u16> = Vec::new();
        let mut cur: Option<u32> = None;
        for &b in body {
            match b {
                b'0'..=b'9' => cur = Some(cur.unwrap_or(0) * 10 + (b - b'0') as u32).map(|v| v.min(65535)),
                b';' => { params.push(cur.unwrap_or(0) as u16); cur = None; }
                b':' => { /* sub-parameter: treat like ';' */ params.push(cur.unwrap_or(0) as u16); cur = None; }
                _ => {}
            }
        }
        if cur.is_some() || fin == b'm' && body.last() == Some(&b';') { params.push(cur.unwrap_or(0) as u16); }
        let p = |i: usize, d: u16| -> usize { params.get(i).copied().filter(|&v| v != 0).unwrap_or(d) as usize };

        if private {
            if fin == b'h' || fin == b'l' {
                let on = fin == b'h';
                for &m in &params {
                    match m {
                        7 => self.autowrap = on,
                        47 | 1047 | 1049 => {
                            self.alt = on;
                            if !on { self.dirty = vec![true; self.rows]; }
                        }
                        _ => {}
                    }
                }
            }
            return;
        }
        match fin {
            b'm' => { let mut a = (*self.attr).clone(); a.apply(&params); self.attr = Rc::new(a); }
            b'A' => { let r = self.row.saturating_sub(p(0, 1)); self.set_cursor(r, self.col); }
            b'B' => { let r = self.row + p(0, 1); self.set_cursor(r, self.col); }
            b'C' => { let c = self.col + p(0, 1); self.set_cursor(self.row, c); }
            b'D' => { let c = self.col.saturating_sub(p(0, 1)); self.set_cursor(self.row, c); }
            b'E' => { let r = self.row + p(0, 1); self.set_cursor(r, 0); }
            b'F' => { let r = self.row.saturating_sub(p(0, 1)); self.set_cursor(r, 0); }
            b'G' | b'`' => { let c = p(0, 1) - 1; self.set_cursor(self.row, c); }
            b'd' => { let r = p(0, 1) - 1; self.set_cursor(r, self.col); }
            b'H' | b'f' => { self.set_cursor(p(0, 1) - 1, p(1, 1) - 1); }
            b'J' => {
                let (r, c) = (self.row, self.col);
                match params.first().copied().unwrap_or(0) {
                    0 => { self.erase_cells(r, c, self.cols); self.erase_rows(r + 1, self.rows); }
                    1 => { self.erase_rows(0, r); self.erase_cells(r, 0, c + 1); }
                    _ => self.erase_rows(0, self.rows),
                }
                self.pending_wrap = false;
            }
            b'K' => {
                let (r, c) = (self.row, self.col);
                match params.first().copied().unwrap_or(0) {
                    0 => self.erase_cells(r, c, self.cols),
                    1 => self.erase_cells(r, 0, c + 1),
                    _ => self.erase_cells(r, 0, self.cols),
                }
                self.pending_wrap = false;
            }
            b'X' => { let (r, c) = (self.row, self.col); let n = p(0, 1); self.erase_cells(r, c, c + n); self.pending_wrap = false; }
            b'@' => {
                let (r, c) = (self.row, self.col); let n = p(0, 1).min(self.cols - c);
                let blank = self.blank();
                for _ in 0..n { self.grid[r].insert(c, blank.clone()); self.grid[r].pop(); }
                self.dirty[r] = true; self.pending_wrap = false;
            }
            b'P' => {
                let (r, c) = (self.row, self.col); let n = p(0, 1).min(self.cols - c);
                let blank = self.blank();
                for _ in 0..n { self.grid[r].remove(c); self.grid[r].push(blank.clone()); }
                self.dirty[r] = true; self.pending_wrap = false;
            }
            b'L' => {
                let n = p(0, 1);
                if self.row >= self.top && self.row <= self.bottom {
                    let (t, r) = (self.top, self.row); self.top = r; self.scroll_down(n); self.top = t;
                }
                self.col = 0; self.pending_wrap = false;
            }
            b'M' => {
                let n = p(0, 1);
                if self.row >= self.top && self.row <= self.bottom {
                    let (t, r) = (self.top, self.row); self.top = r; self.scroll_up(n); self.top = t;
                }
                self.col = 0; self.pending_wrap = false;
            }
            b'S' => self.scroll_up(p(0, 1)),
            b'T' => self.scroll_down(p(0, 1)),
            b'r' => {
                let t = p(0, 1) - 1; let b = p(1, self.rows as u16) - 1;
                if t < b && b < self.rows { self.top = t; self.bottom = b; } else { self.top = 0; self.bottom = self.rows - 1; }
                self.set_cursor(0, 0);
            }
            b's' => self.saved = (self.row, self.col, self.attr.clone()),
            b'u' => { let (r, c, a) = self.saved.clone(); self.set_cursor(r, c); self.attr = a; }
            _ => {}
        }
    }

    // -- repaint -------------------------------------------------------------

    /// Emit repaint escapes for dirty rows. Returns bytes to append to stdout.
    fn repaint(&mut self, out: &mut Vec<u8>) {
        if !self.enabled || self.alt || self.pending_wrap { return; }
        let pal = palette();
        let mut text = String::new();
        let mut cell_of: Vec<usize> = Vec::new(); // byte offset -> cell index
        let mut desired: Vec<Color> = Vec::new();
        let mut wrote = false;
        for r in 0..self.rows {
            if !self.dirty[r] { continue; }
            self.dirty[r] = false;
            text.clear(); cell_of.clear();
            for (ci, cell) in self.grid[r].iter().enumerate() {
                if cell.cont { continue; }
                let start = text.len();
                text.push(cell.ch);
                if let Some(z) = &cell.zw { text.push_str(z); }
                for _ in start..text.len() { cell_of.push(ci); }
            }
            cell_of.push(self.cols);
            self.spans_buf.clear();
            spans(&text, &mut self.spans_buf);
            desired.clear(); desired.resize(self.cols, Color::None);
            for &(s, e, color) in &self.spans_buf {
                let (cs, ce) = (cell_of[s], cell_of[e]);
                for d in desired.iter_mut().take(ce).skip(cs) { *d = color; }
            }
            // wide char continuation cells follow their head
            for c in 1..self.cols { if self.grid[r][c].cont { desired[c] = desired[c - 1]; } }
            let mut c = 0;
            while c < self.cols {
                if desired[c] == self.grid[r][c].shown || self.grid[r][c].cont { c += 1; continue; }
                // run of cells to rewrite
                let start = c;
                let mut last_attr: Option<(Rc<Attr>, Color)> = None;
                let mut seg = String::new();
                while c < self.cols && (desired[c] != self.grid[r][c].shown || self.grid[r][c].cont) {
                    let cell = &self.grid[r][c];
                    if !cell.cont {
                        let need = match &last_attr {
                            Some((a, col)) => !Rc::ptr_eq(a, &cell.attr) && **a != *cell.attr || *col != desired[c],
                            None => true,
                        };
                        if need {
                            seg.push_str(&cell.attr.render(&pal[desired[c] as usize]));
                            last_attr = Some((cell.attr.clone(), desired[c]));
                        }
                        seg.push(cell.ch);
                        if let Some(z) = &cell.zw { seg.push_str(z); }
                    }
                    self.grid[r][c].shown = desired[c];
                    c += 1;
                }
                let _ = write!(out, "\x1b[{};{}H", r + 1, start + 1);
                out.extend_from_slice(seg.as_bytes());
                wrote = true;
            }
        }
        if wrote {
            let _ = write!(out, "\x1b[{};{}H", self.row + 1, self.col + 1);
            out.extend_from_slice(self.attr.render("").as_bytes());
        }
    }

    /// Debug/selftest: render the whole screen inline with colours.
    fn render_inline(&self) -> String {
        let pal = palette();
        let mut s = String::new();
        let mut spans_buf = Vec::new();
        let last_row = (0..self.rows).rev().find(|&r| self.grid[r].iter().any(|c| c.ch != ' ')).map_or(0, |r| r + 1);
        for r in 0..last_row {
            let mut text = String::new();
            let mut cell_of = Vec::new();
            for (ci, cell) in self.grid[r].iter().enumerate() {
                if cell.cont { continue; }
                let st = text.len(); text.push(cell.ch);
                if let Some(z) = &cell.zw { text.push_str(z); }
                for _ in st..text.len() { cell_of.push(ci); }
            }
            cell_of.push(self.cols);
            spans_buf.clear(); spans(&text, &mut spans_buf);
            let mut desired = vec![Color::None; self.cols];
            for &(a, b, col) in &spans_buf { for d in desired.iter_mut().take(cell_of[b]).skip(cell_of[a]) { *d = col; } }
            let end = self.grid[r].iter().rposition(|c| c.ch != ' ').map_or(0, |i| i + 1);
            let mut last: Option<(Rc<Attr>, Color)> = None;
            for c in 0..end {
                let cell = &self.grid[r][c];
                if cell.cont { continue; }
                let need = match &last { Some((a, col)) => **a != *cell.attr || *col != desired[c], None => true };
                if need { s.push_str(&cell.attr.render(&pal[desired[c] as usize])); last = Some((cell.attr.clone(), desired[c])); }
                s.push(cell.ch);
                if let Some(z) = &cell.zw { s.push_str(z); }
            }
            s.push_str("\x1b[0m\n");
        }
        s
    }
}

// ---- entry -----------------------------------------------------------------

const SAMPLE: &str = "Ran git diff --stat && git status --short\r\n\
Ran git diff -- crates/ts_checker/src/semantic/assignment.rs | sed -n '1,300p'\r\n\
Run git status to see changes, then:\r\n\
  git commit -m \"fix: pty filter\" --no-verify\r\n\
  npm install --save-dev vitest and restart the server.\r\n\
Use \x1b[38;2;95;179;217mclaude --rc \"my-project\"\x1b[39m from the project dir.\r\n\
Plain prose with the word node in it, and cd ~/Documents/codes/packages.\r\n\
\x1b[1m● streamed:\x1b[22m Ran git\x1b[0m";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--selftest") {
        let mut sc = Screen::new(40, 200);
        sc.feed(SAMPLE.as_bytes());
        // simulate Claude Code appending to an already-drawn line in pieces
        for piece in [" sta", "tus --sho", "rt && git diff"] { sc.feed(piece.as_bytes()); }
        let mut so = std::io::stdout().lock();
        let _ = so.write_all(sc.render_inline().as_bytes());
        return;
    }
    eprintln!("claude-hl: only --selftest is implemented so far");
    std::process::exit(2);
}
