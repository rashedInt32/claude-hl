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
        let pal = palette();
        let mut so = std::io::stdout().lock();
        let mut sp = Vec::new();
        for line in SAMPLE.split("\r\n") {
            sp.clear();
            spans(line, &mut sp);
            let mut pos = 0;
            for &(s, e, color) in &sp {
                let _ = write!(so, "{}{}{}\x1b[39m", &line[pos..s], pal[color as usize], &line[s..e]);
                pos = e;
            }
            let _ = writeln!(so, "{}", &line[pos..]);
        }
        return;
    }
    eprintln!("claude-hl: only --selftest is implemented so far");
    std::process::exit(2);
}
