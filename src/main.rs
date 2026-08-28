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
use std::ffi::CString;
use std::io::Write;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

// ---- palette ---------------------------------------------------------------

struct Theme {
    cmd: &'static str, sub: &'static str, flag: &'static str, string: &'static str, path: &'static str, op: &'static str,
    /// default foreground remaps (from, to), see `remaps()`
    remap: &'static [(&'static str, &'static str)],
}

/// Claude Code's inline code always uses the stock dark `permission` colour
/// (the markdown renderer resolves the theme by name, bypassing custom
/// themes), so both themes lift that lavender a little by default.
const STOCK_CODESPAN: &str = "b1b9f9";

/// Stock dark `secondaryText` (tool output, timers, recaps) — same
/// resolve-by-name bug, so custom-theme overrides never reach it.
const STOCK_SECONDARY: &str = "999999";
/// nvim rose-pine comment colour; what secondary text should look like.
const NVIM_COMMENT: &str = "7a9a9a";

const THEME_ROSE: Theme = Theme {
    cmd: "9ccfd8", sub: "c4a7e7", flag: "ebbcba", string: "f6c177", path: "e0def4", op: "9ccfd8",
    remap: &[(STOCK_CODESPAN, "c4a7e7"), (STOCK_SECONDARY, NVIM_COMMENT)],
};
const THEME_CODEX: Theme = Theme {
    cmd: "6fb3ff", sub: "7fc8b8", flag: "e78fc7", string: "e5c07b", path: "d0d0d0", op: "6fb3ff",
    remap: &[(STOCK_CODESPAN, "a99cff"), (STOCK_SECONDARY, NVIM_COMMENT)],
};

/// Colour classes; index 0 = "no override".
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Color { None = 0, Cmd, Sub, Flag, Str, Path, Op }

fn rgb(h: &str) -> String {
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(255);
    format!("\x1b[38;2;{};{};{}m", c(0), c(2), c(4))
}

/// SGR strings indexed by Color.
fn theme() -> &'static Theme {
    match std::env::var("CLAUDE_HL_THEME").as_deref() {
        Ok("rose") => &THEME_ROSE,
        _ => &THEME_CODEX,
    }
}

fn palette() -> &'static [String; 7] {
    static P: OnceLock<[String; 7]> = OnceLock::new();
    P.get_or_init(|| {
        let t = theme();
        [String::new(), rgb(t.cmd), rgb(t.sub), rgb(t.flag), rgb(t.string), rgb(t.path), rgb(t.op)]
    })
}

/// Foreground remaps: any cell the app drew in the first colour is shown in
/// the second. The theme supplies defaults; `CLAUDE_HL_REMAP=rrggbb=rrggbb,...`
/// adds to or overrides them (`CLAUDE_HL_REMAP=` empty disables all).
/// Returns (exact SGR fg params to match, SGR to emit).
fn remaps() -> &'static Vec<(String, String)> {
    static R: OnceLock<Vec<(String, String)>> = OnceLock::new();
    R.get_or_init(|| {
        let mut v: Vec<(String, String)> = Vec::new();
        let fg = |h: &str| {
            let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
            format!("38;2;{};{};{}", c(0), c(2), c(4))
        };
        let env = std::env::var("CLAUDE_HL_REMAP").ok();
        if env.is_none() {
            for (from, to) in theme().remap { v.push((fg(from), rgb(to))); }
        }
        if let Some(spec) = env {
            for (from, to) in theme().remap { v.push((fg(from), rgb(to))); }
            for pair in spec.split(',') {
                let Some((from, to)) = pair.trim().split_once('=') else { continue };
                let (from, to) = (from.trim().trim_start_matches('#'), to.trim().trim_start_matches('#'));
                if from.len() != 6 || to.len() != 6 { continue; }
                let key = fg(from);
                if let Some(e) = v.iter_mut().find(|(k, _)| *k == key) { e.1 = rgb(to); }
                else if v.len() < 200 { v.push((key, rgb(to))); }
            }
            if spec.trim().is_empty() { v.clear(); }
        }
        v
    })
}

/// SGR string for a colour code.
fn code_sgr(code: u8) -> &'static str {
    if code < 16 { &palette()[code as usize] } else { &remaps()[code as usize - 16].1 }
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
        if j < n && t[j].is_ascii_alphanumeric() {
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

/// Does the byte at `i` open a quoted run? A double quote always does. An
/// apostrophe only does at a word edge, so prose like `isn't` keeps its
/// letters instead of opening a string that swallows the rest of the row.
fn opens_quote(t: &[u8], i: usize) -> bool {
    match t[i] {
        b'"' => true,
        b'\'' => i == 0 || matches!(t[i - 1], b' ' | b'\t' | b'=' | b'(' | b'"'),
        _ => false,
    }
}

/// Index just past the run opened at `i`, or None when nothing closes it on
/// this row. An unmatched quote stays an ordinary byte: skipping to the end of
/// the row on a lone apostrophe would unpaint every command after it.
fn quoted_end(t: &[u8], i: usize) -> Option<usize> {
    if !opens_quote(t, i) { return None; }
    let (q, n) = (t[i], t.len());
    let mut j = i + 1;
    while j < n {
        match t[j] {
            b'\\' if q == b'"' && j + 1 < n => j += 2,
            c if c == q => return Some(j + 1),
            _ => j += 1,
        }
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
        // a command word inside a string literal is data, not a command
        if let Some(e) = quoted_end(t, i) { i = e; continue; }
        let start = i;
        while i < n && !is_space(t[i]) {
            match quoted_end(t, i) { Some(e) => i = e, None => i += 1 }
        }
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
struct Cell { ch: char, zw: Option<Box<str>>, attr: Rc<Attr>, cont: bool, shown: u8 }

fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x300 { return 1; }
    if matches!(u, 0x300..=0x36F | 0x200B..=0x200F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0xE0100..=0xE01EF) { return 0; }
    if matches!(u,
        // East Asian Wide / Fullwidth
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF |
        0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 |
        0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD |
        // Emoji_Presentation=Yes in the BMP
        0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615 |
        0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 |
        0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B |
        0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF |
        0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 |
        // Emoji_Presentation=Yes in the SMP
        0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A | 0x1F1E6..=0x1F1FF | 0x1F201 | 0x1F21A | 0x1F22F |
        0x1F232..=0x1F236 | 0x1F238..=0x1F23A | 0x1F250..=0x1F251 | 0x1F300..=0x1F320 | 0x1F32D..=0x1F335 |
        0x1F337..=0x1F37C | 0x1F37E..=0x1F393 | 0x1F3A0..=0x1F3CA | 0x1F3CF..=0x1F3D3 | 0x1F3E0..=0x1F3F0 |
        0x1F3F4 | 0x1F3F8..=0x1F43E | 0x1F440 | 0x1F442..=0x1F4FC | 0x1F4FF..=0x1F53D | 0x1F54B..=0x1F54E |
        0x1F550..=0x1F567 | 0x1F57A | 0x1F595..=0x1F596 | 0x1F5A4 | 0x1F5FB..=0x1F64F | 0x1F680..=0x1F6C5 |
        0x1F6CC | 0x1F6D0..=0x1F6D2 | 0x1F6D5..=0x1F6D7 | 0x1F6DC..=0x1F6DF | 0x1F6EB..=0x1F6EC |
        0x1F6F4..=0x1F6FC | 0x1F7E0..=0x1F7EB | 0x1F7F0 | 0x1F90C..=0x1F93A | 0x1F93C..=0x1F945 |
        0x1F947..=0x1F9FF | 0x1FA70..=0x1FA7C | 0x1FA80..=0x1FA88 | 0x1FA90..=0x1FABD | 0x1FABF..=0x1FAC5 |
        0x1FACE..=0x1FADB | 0x1FAE0..=0x1FAE8 | 0x1FAF0..=0x1FAF8
    ) { return 2; }
    1
}

/// Text-default emoji that terminals such as Ghostty, kitty and iTerm2 render
/// double-width once U+FE0F follows them (e.g. ⚠️ ❤️ ☑️ ↔️).
fn vs16_widens(c: char) -> bool {
    let u = c as u32;
    matches!(u, 0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x2199 | 0x21A9..=0x21AA |
        0x2300..=0x23FF | 0x24C2 | 0x25AA..=0x25FE | 0x2600..=0x27BF | 0x2934..=0x2935 | 0x2B05..=0x2B07 |
        0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x3030 | 0x303D | 0x3297 | 0x3299 | 0x1F000..=0x1FAFF)
        && char_width(c) == 1
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
    /// main-screen state parked while the app is on the alternate screen
    main_saved: Option<(Vec<Vec<Cell>>, Vec<bool>, usize, usize, usize, usize, bool)>,
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
        let blank = Cell { ch: ' ', zw: None, attr: attr.clone(), cont: false, shown: 0 };
        Screen {
            rows, cols,
            grid: vec![vec![blank; cols]; rows],
            row: 0, col: 0,
            saved: (0, 0, attr.clone()),
            top: 0, bottom: rows.saturating_sub(1),
            attr, pending_wrap: false, autowrap: true, alt: false, enabled: true,
            dirty: vec![false; rows],
            main_saved: None,
            pst: PState::Ground, csi: Vec::new(), utf8: Vec::new(),
            spans_buf: Vec::new(),
        }
    }

    fn blank(&self) -> Cell { Cell { ch: ' ', zw: None, attr: self.attr.clone(), cont: false, shown: 0 } }

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
        // the parked main screen must follow the new size too, else leaving
        // the alt screen later would find nothing to restore
        if let Some((grid, dirty, r, c, _t, _b, pw)) = self.main_saved.take() {
            let mut d = dirty; d.resize(rows, false);
            self.main_saved = Some((Self::resize_grid(&grid, &blank, rows, cols), d,
                r.min(rows.saturating_sub(1)), c.min(cols.saturating_sub(1)), 0, rows.saturating_sub(1), pw));
        }
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
                cell.shown = 0;
                let widen = c == '\u{FE0F}' && vs16_widens(cell.ch) && !self.pending_wrap
                    && col + 1 < self.cols && !self.grid[row][col + 1].cont;
                if widen {
                    // Ghostty-style: the glyph becomes wide, the cursor moves one right
                    let attr = self.grid[row][col].attr.clone();
                    self.grid[row][col + 1] = Cell { ch: ' ', zw: None, attr, cont: true, shown: 0 };
                    if self.col + 1 >= self.cols { self.col = self.cols - 1; self.pending_wrap = true; }
                    else { self.col += 1; }
                }
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
        self.grid[row][self.col] = Cell { ch: c, zw: None, attr: attr.clone(), cont: false, shown: 0 };
        if w == 2 && self.col + 1 < self.cols {
            self.grid[row][self.col + 1] = Cell { ch: ' ', zw: None, attr, cont: true, shown: 0 };
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
                            if on && !self.alt {
                                // park the main screen; the alt screen starts blank
                                let blank = self.blank();
                                let grid = std::mem::replace(&mut self.grid, vec![vec![blank; self.cols]; self.rows]);
                                let dirty = std::mem::replace(&mut self.dirty, vec![false; self.rows]);
                                self.main_saved = Some((grid, dirty, self.row, self.col, self.top, self.bottom, self.pending_wrap));
                                self.top = 0; self.bottom = self.rows - 1;
                                self.alt = true;
                            } else if !on && self.alt {
                                // the terminal restores the main screen exactly as it was,
                                // including our earlier repaints, so nothing is dirty
                                match self.main_saved.take() {
                                    Some((grid, dirty, r, c, t, b, pw)) => {
                                        self.grid = grid; self.dirty = dirty;
                                        self.row = r.min(self.rows - 1); self.col = c.min(self.cols - 1);
                                        self.top = t; self.bottom = b; self.pending_wrap = pw;
                                    }
                                    None => {
                                        // 1049l without a matching 1049h: the main screen
                                        // content and cursor are unknown; model blank, paint nothing
                                        let blank = self.blank();
                                        self.grid = vec![vec![blank; self.cols]; self.rows];
                                        self.dirty = vec![false; self.rows];
                                        self.top = 0; self.bottom = self.rows - 1;
                                        self.enabled = false;
                                    }
                                }
                                self.alt = false;
                            }
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
        if (!self.enabled && !self.alt) || self.pending_wrap { return; }
        // a read boundary can fall inside an escape sequence or a multi-byte
        // char; injecting bytes there would corrupt it. Rows stay dirty and
        // paint on the next chunk.
        if self.pst != PState::Ground || !self.utf8.is_empty() { return; }
        let mut text = String::new();
        let mut cell_of: Vec<usize> = Vec::new(); // byte offset -> cell index
        let mut desired: Vec<u8> = Vec::new();
        let rm = remaps();
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
            desired.clear(); desired.resize(self.cols, 0);
            if !rm.is_empty() {
                for (ci, cell) in self.grid[r].iter().enumerate() {
                    if let Some(k) = rm.iter().position(|(from, _)| *from == cell.attr.fg) { desired[ci] = 16 + k as u8; }
                }
            }
            for &(s, e, color) in &self.spans_buf {
                let (cs, ce) = (cell_of[s], cell_of[e]);
                for d in desired.iter_mut().take(ce).skip(cs) { *d = color as u8; }
            }
            // wide char continuation cells follow their head
            for c in 1..self.cols { if self.grid[r][c].cont { desired[c] = desired[c - 1]; } }
            let mut c = 0;
            while c < self.cols {
                if desired[c] == self.grid[r][c].shown || self.grid[r][c].cont { c += 1; continue; }
                // run of cells to rewrite
                let start = c;
                let mut last_attr: Option<(Rc<Attr>, u8)> = None;
                let mut seg = String::new();
                while c < self.cols && (desired[c] != self.grid[r][c].shown || self.grid[r][c].cont) {
                    let cell = &self.grid[r][c];
                    if !cell.cont {
                        let need = match &last_attr {
                            Some((a, col)) => !Rc::ptr_eq(a, &cell.attr) && **a != *cell.attr || *col != desired[c],
                            None => true,
                        };
                        if need {
                            seg.push_str(&cell.attr.render(code_sgr(desired[c])));
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

// ---- PTY plumbing ----------------------------------------------------------

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) { WINCH.store(true, Ordering::SeqCst); }

fn winsize(fd: libc::c_int) -> Option<libc::winsize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 { Some(ws) } else { None }
    }
}

fn write_all(fd: libc::c_int, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const _, buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted || e.kind() == std::io::ErrorKind::WouldBlock { continue; }
            return false;
        }
        buf = &buf[n as usize..];
    }
    true
}

/// Ask the real terminal where the cursor is (DSR). Returns (row, col, leftover stdin bytes).
fn query_cursor(stdin: libc::c_int, stdout: libc::c_int) -> (Option<(usize, usize)>, Vec<u8>) {
    write_all(stdout, b"\x1b[6n");
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 256];
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() { break; }
        let mut fds = [libc::pollfd { fd: stdin, events: libc::POLLIN, revents: 0 }];
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 1, left.as_millis() as i32) };
        if r <= 0 { if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { continue; } break; }
        let n = unsafe { libc::read(stdin, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 { break; }
        acc.extend_from_slice(&buf[..n as usize]);
        // look for ESC [ row ; col R anywhere in what arrived; other
        // sequences (keys, focus events) may sit before or after it
        for s in (0..acc.len()).filter(|&i| acc[i] == 0x1b) {
            let tail = &acc[s..];
            if tail.len() < 2 || tail[1] != b'[' { continue; }
            let e = match tail[2..].iter().position(|&b| !(b.is_ascii_digit() || b == b';')) {
                Some(k) => k + 2,
                None => continue,
            };
            if tail[e] != b'R' { continue; }
            let body = std::str::from_utf8(&tail[2..e]).unwrap_or("");
            let mut it = body.split(';').map(|x| x.parse::<usize>().unwrap_or(1));
            let row = it.next().unwrap_or(1).max(1) - 1;
            let col = it.next().unwrap_or(1).max(1) - 1;
            let mut left = acc[..s].to_vec();
            left.extend_from_slice(&tail[e + 1..]);
            return (Some((row, col)), left);
        }
    }
    (None, acc)
}

fn run(argv: &[String]) -> i32 {
    let stdin = libc::STDIN_FILENO;
    let stdout = libc::STDOUT_FILENO;
    let isatty = unsafe { libc::isatty(stdin) } == 1;
    let out_tty = unsafe { libc::isatty(stdout) } == 1;
    let ws = if isatty { winsize(stdin) } else { None };
    let (rows, cols) = ws.map_or((24, 80), |w| (w.ws_row.max(1) as usize, w.ws_col.max(1) as usize));

    let mut old: Option<libc::termios> = None;
    let mut screen = Screen::new(rows, cols);
    if !isatty || !out_tty {
        // no terminal to measure or to draw on: pure passthrough
        screen.enabled = false;
    }
    // raw mode before spawning so the DSR reply below never echoes
    if isatty && out_tty {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(stdin, &mut t) == 0 {
                old = Some(t);
                let mut raw = t;
                libc::cfmakeraw(&mut raw);
                libc::tcsetattr(stdin, libc::TCSAFLUSH, &raw);
            }
        }
    }

    let cargs: Vec<CString> = argv.iter().map(|a| CString::new(a.as_str()).unwrap()).collect();
    let mut master: libc::c_int = 0;
    let mut wsz = ws.unwrap_or(libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 });
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &mut wsz) };
    if pid < 0 {
        if let Some(t) = old { unsafe { libc::tcsetattr(stdin, libc::TCSADRAIN, &t); } }
        eprintln!("claude-hl: forkpty failed: {}", std::io::Error::last_os_error());
        return 1;
    }
    if pid == 0 {
        let mut ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        unsafe { libc::execvp(ptrs[0], ptrs.as_ptr()); libc::_exit(127); }
    }
    if isatty {
        unsafe { libc::signal(libc::SIGWINCH, on_winch as extern "C" fn(libc::c_int) as libc::sighandler_t); }
    }

    // cursor query after spawning: the child boots during the terminal
    // round-trip, so a slow (or silent, 400ms) reply costs no wall time.
    // query_cursor strips the reply from stdin before it can reach the
    // child; the child's output waits unread in the pty buffer meanwhile.
    let mut leftover = Vec::new();
    if isatty && out_tty {
        let (pos, rest) = query_cursor(stdin, stdout);
        match pos {
            Some((r, c)) => screen.set_cursor(r, c),
            None => {
                // unknown cursor position: never guess, just pass through
                screen.enabled = false;
                eprintln!("claude-hl: terminal did not answer cursor query; highlighting disabled");
            }
        }
        leftover = rest;
    }
    if !leftover.is_empty() { write_all(master, &leftover); }

    let mut dump = std::env::var("CLAUDE_HL_DUMP").ok().and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
    let mut buf = vec![0u8; 65536];
    let mut out: Vec<u8> = Vec::with_capacity(131072);
    let mut watch_stdin = true;
    loop {
        if WINCH.swap(false, Ordering::SeqCst) {
            if let Some(w) = winsize(stdin) {
                unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &w); libc::kill(pid, libc::SIGWINCH); }
                screen.resize(w.ws_row.max(1) as usize, w.ws_col.max(1) as usize);
            }
        }
        let mut fds = [
            libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if watch_stdin { stdin } else { -1 }, events: libc::POLLIN, revents: 0 },
        ];
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { continue; }
            break;
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 { break; }
            let data = &buf[..n as usize];
            if let Some(f) = dump.as_mut() { let _ = f.write_all(data); }
            out.clear();
            // paint before the end of a synchronized-update block so the
            // terminal shows text and colour in the same frame
            const SYNC_END: &[u8] = b"\x1b[?2026l";
            let split = data.windows(SYNC_END.len()).rposition(|w| w == SYNC_END);
            let (head, tail) = match split { Some(i) => (&data[..i], &data[i..]), None => (data, &data[data.len()..]) };
            out.extend_from_slice(head);
            screen.feed(head);
            screen.repaint(&mut out);
            if !tail.is_empty() {
                out.extend_from_slice(tail);
                screen.feed(tail);
                screen.repaint(&mut out);
            }
            if !write_all(stdout, &out) { break; }
        }
        // macOS poll() reports POLLNVAL for /dev/null; treat it as EOF
        let stdin_ev = libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
        if watch_stdin && fds[1].revents & stdin_ev != 0 {
            let n = if fds[1].revents & libc::POLLNVAL != 0 { 0 }
                    else { unsafe { libc::read(stdin, buf.as_mut_ptr() as *mut _, buf.len()) } };
            if n <= 0 {
                // stdin closed: hand the child an EOF (its VEOF char) and keep
                // draining its output, else it blocks on the pty write and
                // never exits (macOS).
                unsafe {
                    let mut t: libc::termios = std::mem::zeroed();
                    if libc::tcgetattr(master, &mut t) == 0 {
                        let eof = t.c_cc[libc::VEOF];
                        if eof != 0 && eof != 0xff { write_all(master, &[eof]); }
                    }
                }
                watch_stdin = false;
                continue;
            }
            write_all(master, &buf[..n as usize]);
        }
    }

    if let Some(t) = old { unsafe { libc::tcsetattr(stdin, libc::TCSADRAIN, &t); } }
    let mut status: libc::c_int = 0;
    let w = unsafe { libc::waitpid(pid, &mut status, 0) };
    if w < 0 { return 0; }
    if libc::WIFEXITED(status) { return libc::WEXITSTATUS(status); }
    1
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
    let cmd = std::env::var("CLAUDE_HL_CMD").unwrap_or_else(|_| "claude".to_string());
    let mut argv = vec![cmd];
    argv.extend(args);
    std::process::exit(run(&argv));
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Render one row's spans as `text=KIND` pairs, in order, so a failing
    /// assert prints what was painted instead of a byte-range diff.
    fn painted(line: &str) -> Vec<(&str, &'static str)> {
        let mut v = Vec::new();
        spans(line, &mut v);
        v.sort_by_key(|s| s.0);
        v.into_iter()
            .map(|(s, e, c)| {
                let kind = match c {
                    Color::Cmd => "cmd", Color::Sub => "sub", Color::Flag => "flag",
                    Color::Str => "str", Color::Path => "path", Color::Op => "op",
                    Color::None => "none",
                };
                (&line[s..e], kind)
            })
            .collect()
    }

    // -- a command word inside a string literal is data ----------------------

    #[test]
    fn quoted_argument_is_never_tokenised_as_shell() {
        // `cd` and `&&` live inside the quotes; painting them made the string
        // look like a second command line.
        assert_eq!(painted(r#"ssh box "cd /srv && ./run.sh --dry-run 'a b c'""#), []);
    }

    #[test]
    fn flag_inside_a_string_stays_string() {
        assert_eq!(
            painted(r#"echo "use --force to override""#),
            [("echo", "cmd"), (r#""use --force to override""#, "str")]
        );
    }

    #[test]
    fn nested_quotes_do_not_split_the_outer_string() {
        // The inner apostrophes must not end the double-quoted run, and
        // `cd`/`AS` inside it must not be picked up as a second command.
        assert_eq!(
            painted(r#"echo "SELECT 'off-state' AS s WHERE cd = 'x';""#),
            [("echo", "cmd"), (r#""SELECT 'off-state' AS s WHERE cd = 'x';""#, "str")]
        );
    }

    // -- an apostrophe in prose is not a quote -------------------------------

    #[test]
    fn lone_apostrophe_does_not_swallow_the_rest_of_the_row() {
        assert_eq!(
            painted("don't run git status --short"),
            [("git", "cmd"), ("status", "sub"), ("--short", "flag")]
        );
    }

    #[test]
    fn two_apostrophes_in_prose_do_not_form_a_quoted_run() {
        // A naive scanner reads `'t run git'` as a string and loses `git`.
        assert_eq!(
            painted("it isn't the agent's job to run git status"),
            [("git", "cmd"), ("status", "sub")]
        );
    }

    #[test]
    fn quoted_glob_after_equals_still_reads_as_a_string() {
        assert_eq!(
            painted("tar -czvf backup.tgz --exclude='*.tmp' -- ./data"),
            [("tar", "cmd"), ("-czvf", "flag"), ("backup.tgz", "path"),
             ("--exclude='*.tmp'", "flag"), ("--", "flag"), ("./data", "path")]
        );
    }

    // -- numeric short flags -------------------------------------------------

    #[test]
    fn numeric_short_flag_keeps_the_run_going() {
        // `-0` used to return None, ending the arg loop: `xargs` kept its
        // colour but every argument after it went plain.
        assert_eq!(
            painted("xargs -0 -n1 basename"),
            [("xargs", "cmd"), ("-0", "flag"), ("-n1", "flag")]
        );
    }

    #[test]
    fn numeric_flag_as_first_arg_keeps_the_command_painted() {
        // Same cause, worse symptom: None on the first argument left nargs at
        // 0, so the command span was discarded and the row rendered plain.
        assert_eq!(
            painted("kill -9 -- -1234"),
            [("kill", "cmd"), ("-9", "flag"), ("--", "flag"), ("-1234", "flag")]
        );
    }

    // -- guards that must not regress ----------------------------------------

    #[test]
    fn leading_command_is_found_before_any_quote() {
        assert_eq!(
            painted(r#"git commit -m "fix: 86 push guard""#),
            [("git", "cmd"), ("commit", "sub"), ("-m", "flag"), (r#""fix: 86 push guard""#, "str")]
        );
    }

    #[test]
    fn bare_command_word_in_prose_stays_plain() {
        assert_eq!(painted("Plain prose with the word node in it"), []);
    }

    #[test]
    fn pipeline_keeps_both_sides() {
        assert_eq!(
            painted("find . -name '*.sql' -print0 | xargs -0 basename"),
            [("find", "cmd"), (".", "path"), ("-name", "flag"), ("'*.sql'", "str"),
             ("-print0", "flag"), ("|", "op"), ("xargs", "cmd"), ("-0", "flag")]
        );
    }
}
