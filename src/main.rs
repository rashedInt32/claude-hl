//! claude-hl: run a TUI (default: claude) inside a PTY and paint shell
//! commands in its output, Codex-style.
//!
//!   claude-hl [args passed to claude...]
//!   CLAUDE_HL_CMD=codex claude-hl        # wrap something else
//!   CLAUDE_HL_THEME=rose claude-hl       # rose | catppuccin | tokyonight | dracula | gruvbox | nord (default: codex)
//!   CLAUDE_HL_COLORS=cmd=89b4fa,num=fab387 claude-hl   # override single slots of the theme
//!   CLAUDE_HL_COMMANDS="bash sh -make" claude-hl        # add words to the vocabulary, `-word` removes
//!   CLAUDE_HL_DUMP=/path claude-hl       # also append the raw PTY stream to a file (debug)
//!   claude-hl --selftest                 # print sample highlighted text
//!   claude-hl --themes                   # preview every theme
//!   claude-hl --version                  # wrapper version (everything else passes through)
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
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::OnceLock;

// ---- palette ---------------------------------------------------------------

struct Theme {
    cmd: &'static str, sub: &'static str, flag: &'static str, string: &'static str, path: &'static str, op: &'static str,
    num: &'static str, var: &'static str, url: &'static str, comment: &'static str,
    /// default foreground remaps (from, to), see `remaps()`
    remap: &'static [(&'static str, &'static str)],
}

/// Claude Code's inline code always uses the stock dark `permission` colour
/// (the markdown renderer resolves the theme by name, bypassing custom
/// themes), so themes replace that lavender with their own accent.
const STOCK_CODESPAN: &str = "b1b9f9";

/// Stock dark `secondaryText` (tool output, timers, recaps, typed slash
/// commands) — same resolve-by-name bug, so custom-theme overrides never
/// reach it. Themes lift that flat gray into their own tint.
const STOCK_SECONDARY: &str = "999999";
/// nvim rose-pine comment colour; what secondary text should look like.
const NVIM_COMMENT: &str = "7a9a9a";

const THEME_ROSE: Theme = Theme {
    cmd: "9ccfd8", sub: "c4a7e7", flag: "ebbcba", string: "f6c177", path: "e0def4", op: "9ccfd8",
    num: "ea9a97", var: "eb6f92", url: "9ccfd8", comment: "6e6a86",
    remap: &[(STOCK_CODESPAN, "c4a7e7"), (STOCK_SECONDARY, NVIM_COMMENT)],
};
const THEME_CODEX: Theme = Theme {
    cmd: "6fb3ff", sub: "7fc8b8", flag: "e78fc7", string: "e5c07b", path: "d0d0d0", op: "6fb3ff",
    num: "d19a66", var: "98c379", url: "6fb3ff", comment: "7a9a9a",
    remap: &[(STOCK_CODESPAN, "e5c07b"), (STOCK_SECONDARY, NVIM_COMMENT)],
};
const THEME_CATPPUCCIN: Theme = Theme {
    cmd: "89b4fa", sub: "94e2d5", flag: "f5c2e7", string: "f9e2af", path: "cdd6f4", op: "89dceb",
    num: "fab387", var: "a6e3a1", url: "89b4fa", comment: "6c7086",
    remap: &[(STOCK_CODESPAN, "cba6f7"), (STOCK_SECONDARY, "7f849c")],
};
const THEME_TOKYO: Theme = Theme {
    cmd: "7aa2f7", sub: "73daca", flag: "bb9af7", string: "e0af68", path: "c0caf5", op: "7dcfff",
    num: "ff9e64", var: "9ece6a", url: "7aa2f7", comment: "565f89",
    remap: &[(STOCK_CODESPAN, "9d7cd8"), (STOCK_SECONDARY, "737aa2")],
};
const THEME_DRACULA: Theme = Theme {
    cmd: "8be9fd", sub: "50fa7b", flag: "ff79c6", string: "f1fa8c", path: "f8f8f2", op: "bd93f9",
    num: "ffb86c", var: "bd93f9", url: "8be9fd", comment: "6272a4",
    remap: &[(STOCK_CODESPAN, "bd93f9"), (STOCK_SECONDARY, "6272a4")],
};
const THEME_GRUVBOX: Theme = Theme {
    cmd: "83a598", sub: "8ec07c", flag: "d3869b", string: "fabd2f", path: "ebdbb2", op: "fe8019",
    num: "fe8019", var: "b8bb26", url: "83a598", comment: "928374",
    remap: &[(STOCK_CODESPAN, "b8bb26"), (STOCK_SECONDARY, "928374")],
};
const THEME_NORD: Theme = Theme {
    cmd: "88c0d0", sub: "8fbcbb", flag: "b48ead", string: "ebcb8b", path: "eceff4", op: "81a1c1",
    num: "d08770", var: "a3be8c", url: "88c0d0", comment: "616e88",
    remap: &[(STOCK_CODESPAN, "b48ead"), (STOCK_SECONDARY, "616e88")],
};

const THEME_NAMES: &[&str] = &["codex", "rose", "catppuccin", "tokyonight", "dracula", "gruvbox", "nord"];

/// Colour classes; index 0 = "no override". Remap codes start at 16.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
#[allow(dead_code)]
enum Color { None = 0, Cmd, Sub, Flag, Str, Path, Op, Num, Var, Url, Comment }
const NCOLORS: usize = 11;
const REMAP_BASE: u8 = 16;

/// `38;2;r;g;b` for a hex colour (SGR params, no ESC).
fn fg_params(h: &str) -> String {
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(255);
    format!("38;2;{};{};{}", c(0), c(2), c(4))
}

fn valid_hex(h: &str) -> bool { h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()) }

fn theme() -> &'static Theme {
    match std::env::var("CLAUDE_HL_THEME").as_deref() {
        Ok("rose") => &THEME_ROSE,
        Ok("catppuccin") => &THEME_CATPPUCCIN,
        Ok("tokyo") | Ok("tokyonight") => &THEME_TOKYO,
        Ok("dracula") => &THEME_DRACULA,
        Ok("gruvbox") => &THEME_GRUVBOX,
        Ok("nord") => &THEME_NORD,
        Ok(other) if !other.is_empty() && other != "codex" => {
            eprintln!("claude-hl: unknown theme {other:?}, using codex (try --themes)");
            &THEME_CODEX
        }
        _ => &THEME_CODEX,
    }
}

/// SGR params (no ESC/`m`) indexed by Color: the command word is bold, URLs
/// underlined, comments dim. `CLAUDE_HL_COLORS=slot=rrggbb,...` overrides
/// single slots of the chosen theme.
fn palette() -> &'static [String; NCOLORS] {
    static P: OnceLock<[String; NCOLORS]> = OnceLock::new();
    P.get_or_init(|| {
        let t = theme();
        let mut hex = [t.cmd, t.sub, t.flag, t.string, t.path, t.op, t.num, t.var, t.url, t.comment];
        const SLOTS: [&str; 10] = ["cmd", "sub", "flag", "string", "path", "op", "num", "var", "url", "comment"];
        if let Ok(spec) = std::env::var("CLAUDE_HL_COLORS") {
            for pair in spec.split(',') {
                let Some((k, v)) = pair.trim().split_once('=') else { continue };
                let v = v.trim().trim_start_matches('#');
                match (SLOTS.iter().position(|s| *s == k.trim()), valid_hex(v)) {
                    (Some(i), true) => hex[i] = Box::leak(v.to_string().into_boxed_str()),
                    _ => eprintln!("claude-hl: ignoring CLAUDE_HL_COLORS entry {pair:?} (slots: {})", SLOTS.join(" ")),
                }
            }
        }
        let style = |i: usize| match i { 0 => "1;", 8 => "4;", 9 => "2;", _ => "" };
        let mut p: [String; NCOLORS] = Default::default();
        for i in 0..10 { p[i + 1] = format!("{}{}", style(i), fg_params(hex[i])); }
        p
    })
}

/// Foreground remaps: any cell the app drew in the first colour is shown in
/// the second. The theme supplies defaults; `CLAUDE_HL_REMAP=rrggbb=rrggbb,...`
/// adds to or overrides them (`CLAUDE_HL_REMAP=` empty disables all).
/// Returns (exact SGR fg params to match, SGR params to emit).
fn remaps() -> &'static Vec<(String, String)> {
    static R: OnceLock<Vec<(String, String)>> = OnceLock::new();
    R.get_or_init(|| {
        let mut v: Vec<(String, String)> = Vec::new();
        for (from, to) in theme().remap { v.push((fg_params(from), fg_params(to))); }
        if let Ok(spec) = std::env::var("CLAUDE_HL_REMAP") {
            if spec.trim().is_empty() { v.clear(); return v; }
            for pair in spec.split(',') {
                let Some((from, to)) = pair.trim().split_once('=') else { continue };
                let (from, to) = (from.trim().trim_start_matches('#'), to.trim().trim_start_matches('#'));
                if !valid_hex(from) || !valid_hex(to) { continue; }
                let key = fg_params(from);
                if let Some(e) = v.iter_mut().find(|(k, _)| *k == key) { e.1 = fg_params(to); }
                else if v.len() < 200 { v.push((key, fg_params(to))); }
            }
        }
        v
    })
}

/// SGR params for a colour code.
fn code_sgr(code: u8) -> &'static str {
    if code < REMAP_BASE { &palette()[code as usize] } else { &remaps()[(code - REMAP_BASE) as usize].1 }
}

/// The fg Claude Code draws inline code in; a cell in this colour is
/// definitely code, so the tokenizer can be generous there.
fn codespan_fg() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| fg_params(STOCK_CODESPAN))
}

// ---- vocabulary ------------------------------------------------------------

const COMMANDS: &str = "git gh npm npx pnpm yarn bun bunx node deno python python3 pip pip3 uv go cargo \
rustc make cmake brew apt apt-get dnf pacman docker docker-compose kubectl helm \
terraform aws gcloud az ssh scp rsync curl wget tar zip unzip cd ls cat head \
tail less grep rg fd find xargs sed awk sort uniq wc tr cut tee echo printf \
export source chmod chown mkdir rmdir rm cp mv ln touch pwd which env nvim vim \
tmux ghostty claude codex gemini open kill pkill ps lsof jq yq tsc tsx vitest \
jest eslint prettier next vite \
sudo doas nohup just pytest ruff mypy poetry pipx uvx conda mvn gradle dotnet \
swift ruby gem bundle rake rspec php composer psql mysql sqlite3 redis-cli \
mongosh systemctl journalctl launchctl xcodebuild xcrun flutter dart adb \
ffmpeg pandoc openssl gpg shellcheck protoc ansible vagrant nix zig perl lua \
gcc clang ninja bazel \
bash sh zsh fish exec exit man diff patch du df sleep time watch bat eza tree stat \
rustup nvm fnm pm2 podman k9s minikube kind htop nc dig ping ssh-keygen base64 \
shasum sha256sum xxd hyperfine tokei cloc fzf zoxide code cursor pbcopy pbpaste \
xdg-open turbo nx lerna webpack esbuild biome prisma wrangler vercel netlify \
flyctl heroku supabase firebase ngrok mise asdf direnv gdb lldb valgrind strace \
mkcert caddy certbot crontab screen zellij";

/// tools whose bare-word args are subcommands; for others (node, cat, cd...)
/// only flags/paths/strings count, so prose like "node here" stays plain
const SUBCMD_TOOLS: &str = "git gh npm npx pnpm yarn bun bunx deno uv pip pip3 go cargo brew apt apt-get \
dnf pacman docker docker-compose kubectl helm terraform aws gcloud az claude \
codex gemini tmux make jq tsc next vite \
sudo doas nohup poetry pipx conda mvn gradle dotnet swift gem bundle rake \
composer systemctl journalctl launchctl flutter dart adb nix vagrant bazel openssl \
man rustup nvm fnm pm2 podman minikube kind turbo nx lerna prisma wrangler vercel \
netlify flyctl heroku supabase firebase mise asdf direnv crontab";

/// prefix runners: a known command word right after them re-anchors the
/// highlight, so `sudo systemctl ...` paints systemctl as a command again
const CHAIN_TOOLS: &str = "sudo doas env xargs nohup time watch exec";

const STOP_WORDS: &str = "and or then to the a an in on for with is it that this if of at by from so but \
you we i will can should after before when do not into was are has have your our";

/// text right before a command that marks it as a command line rather than
/// prose (Claude Code's tool-call recap, a shell prompt)
const RUNNER_PREFIXES: &[&str] = &["Ran", "Run", "Running", "Bash(", "$", "❯", "›"];

/// bare words accepted after the command (e.g. `push origin main`)
const MAX_SUB: usize = 3;
/// in prose, bare non-subcommand words tolerated before giving up
const MAX_BARE: usize = 3;

struct Vocab {
    commands: HashSet<&'static str>,
    subcmd_tools: HashSet<&'static str>,
    chain_tools: HashSet<&'static str>,
    stop_words: HashSet<&'static str>,
}

fn vocab() -> &'static Vocab {
    static V: OnceLock<Vocab> = OnceLock::new();
    V.get_or_init(|| {
        let mut v = Vocab {
            commands: COMMANDS.split_whitespace().collect(),
            subcmd_tools: SUBCMD_TOOLS.split_whitespace().collect(),
            chain_tools: CHAIN_TOOLS.split_whitespace().collect(),
            stop_words: STOP_WORDS.split_whitespace().collect(),
        };
        // CLAUDE_HL_COMMANDS="bash sh -make": `word` adds, `-word` removes;
        // `word:sub` also accepts bare subcommands after it
        if let Ok(spec) = std::env::var("CLAUDE_HL_COMMANDS") {
            for w in spec.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()) {
                if let Some(rm) = w.strip_prefix('-') {
                    v.commands.remove(rm); v.subcmd_tools.remove(rm); v.chain_tools.remove(rm);
                    continue;
                }
                let (word, sub) = match w.split_once(':') { Some((a, "sub")) => (a, true), _ => (w, false) };
                let word: &'static str = Box::leak(word.to_string().into_boxed_str());
                v.commands.insert(word);
                if sub { v.subcmd_tools.insert(word); }
            }
        }
        v
    })
}

// ---- tokenizer -------------------------------------------------------------

#[derive(PartialEq, Clone, Copy, Debug)]
enum Kind { Ws, Str, Chain, Redirect, Flag, Path, Sub, Word, Num, Var, Url, Comment }

fn is_ws(b: u8) -> bool { b == b' ' || b == b'\t' }
fn is_space(b: u8) -> bool { matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) }
fn is_word(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'_' }
/// chars that may not precede a command word
fn is_cmd_glue(b: u8) -> bool { is_word(b) || matches!(b, b'/' | b'.' | b'@' | b'~' | b'-') }
fn is_path_mark(b: u8) -> bool { matches!(b, b'/' | b'.' | b'~' | b'=' | b':' | b'@' | b'*') }
fn at_boundary(t: &[u8], i: usize) -> bool { i >= t.len() || is_space(t[i]) }
fn is_sentence_punct(b: u8) -> bool { matches!(b, b'.' | b',' | b';' | b':' | b')') }
/// end of a shell word: whitespace or a closing backtick
fn word_end(t: &[u8], mut j: usize) -> usize {
    while j < t.len() && !is_space(t[j]) && t[j] != b'`' { j += 1; }
    j
}

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
    // operators may be glued to what follows (`grep x|wc`, `2>/dev/null`,
    // `<<'EOF'`) as long as the next char is not another operator char
    let op_end = |j: usize| at_boundary(t, j) || !matches!(t[j], b'>' | b'<' | b'&' | b'|' | b';');
    for op in [&b"&&"[..], b"||", b"|", b";"] {
        let j = i + op.len();
        if t[i..].starts_with(op) && (if op == b";" { at_boundary(t, j) } else { op_end(j) }) { return Some((Kind::Chain, j)); }
    }
    for op in [&b"2>&1"[..], b"&>", b"2>", b">>", b">", b"<<", b"<"] {
        let j = i + op.len();
        if t[i..].starts_with(op) && op_end(j) { return Some((Kind::Redirect, j)); }
    }
    if b == b'#' && (i + 1 >= n || is_space(t[i + 1])) {
        let mut j = i;
        while j < n && t[j] != b'`' { j += 1; }
        return Some((Kind::Comment, j));
    }
    if b == b'$' { return Some((Kind::Var, word_end(t, i + 1))); }
    for scheme in [&b"http://"[..], b"https://", b"ssh://", b"git@", b"file://", b"www."] {
        if t[i..].starts_with(scheme) { return Some((Kind::Url, word_end(t, i))); }
    }
    if b == b'-' {
        let mut j = i + 1;
        if j < n && t[j] == b'-' { j += 1; }
        if j < n && t[j].is_ascii_alphabetic() {
            j += 1;
            while j < n && (is_word(t[j]) || t[j] == b'-') { j += 1; }
            // `--flag=value`: the flag ends after `=`, the value is its own token
            if j < n && t[j] == b'=' { j += 1; }
            return Some((Kind::Flag, j));
        }
        // numeric flags: `tail -20`, `head -5`
        if j < n && t[j].is_ascii_digit() {
            let mut k = j + 1;
            while k < n && t[k].is_ascii_digit() { k += 1; }
            if at_boundary(t, k) { return Some((Kind::Flag, k)); }
        }
        // `--` and `-` on their own (`cargo test -- --nocapture`, `cd -`)
        if at_boundary(t, j) { return Some((Kind::Flag, j)); }
    }
    // `chmod +x`, `date +%s`
    if b == b'+' && i + 1 < n && !is_space(t[i + 1]) { return Some((Kind::Flag, word_end(t, i + 1))); }
    // numbers, versions, sizes, durations: 5  1.2.0  v1.2.0  10s  3.5GB  80%
    if b.is_ascii_digit() || (b == b'v' && i + 1 < n && t[i + 1].is_ascii_digit()) {
        let j = word_end(t, i);
        // sentence punctuation stays in the token; `spans` strips it
        let body_end = if j - i > 1 && is_sentence_punct(t[j - 1]) { j - 1 } else { j };
        let body = &t[i + (b == b'v') as usize..body_end];
        let numeric = !body.is_empty() && body.iter().all(|&c| c.is_ascii_digit() || matches!(c, b'.' | b'%' | b'k' | b'K' | b'm' | b'M' | b'g' | b'G' | b'B' | b's' | b'h' | b'd'));
        if numeric { return Some((Kind::Num, j)); }
    }
    {
        let j = word_end(t, i);
        // `PORT=3000`, `GIT_SSH=...`: an environment assignment
        if b.is_ascii_alphabetic() || b == b'_' {
            let k = i + t[i..j].iter().take_while(|&&c| is_word(c)).count();
            if k < j && t[k] == b'=' { return Some((Kind::Var, j)); }
        }
        // `size.` is a word plus a full stop, not a path; `x.txt.` still is
        let body_end = if j - i > 1 && is_sentence_punct(t[j - 1]) { j - 1 } else { j };
        if t[i..body_end].iter().any(|&c| is_path_mark(c)) { return Some((Kind::Path, j)); }
    }
    if b.is_ascii_lowercase() {
        let mut j = i + 1;
        while j < n && (t[j].is_ascii_lowercase() || t[j].is_ascii_digit() || t[j] == b'-') { j += 1; }
        // `size.` at the end of a sentence: keep the punctuation in the token
        // so `spans` sees it and ends the span there
        if j < n && is_sentence_punct(t[j]) && at_boundary(t, j + 1) { j += 1; }
        return Some((Kind::Sub, j));
    }
    // other bare words (HEAD, Makefile, README); only trusted in code context
    if is_word(b) { return Some((Kind::Word, word_end(t, i))); }
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

/// Is the text right before `cs` a runner prefix (`Ran `, `$ `, ...)?
fn has_runner_prefix(text: &str, cs: usize) -> bool {
    let before = text[..cs].trim_end();
    RUNNER_PREFIXES.iter().any(|p| before.ends_with(p))
}

/// Compute colour spans (byte ranges) for one line of text.
///
/// `code[b]` says byte `b` sits in a cell Claude Code drew as inline code.
/// Inside code, or right after a runner prefix, the tokenizer is generous:
/// bare words are arguments and `git status` alone is a command. In prose it
/// demands evidence (a flag, path, string, operator or number), so "make
/// sure", "go ahead" and "next step" stay plain.
fn spans(text: &str, code: &[bool], out: &mut Vec<(usize, usize, Color)>) {
    let t = text.as_bytes();
    let v = vocab();
    let mut pos = 0;
    while let Some((cs, ce)) = find_cmd(t, pos) {
        let mut cmd = &text[cs..ce];
        let mark = out.len();
        out.push((cs, ce, Color::Cmd));
        let in_code = |a: usize, b: usize| code.get(a..b).map_or(false, |c| c.iter().any(|&x| x));
        let runner = has_runner_prefix(text, cs);
        let cmd_in_code = in_code(cs, ce);
        let strong = runner || cmd_in_code;
        let mut i = ce;
        let mut subs = 0;
        let mut nargs = 0;
        let mut bare = 0; // prose-context bare words taken on trust so far
        let mut after_eq = false;
        let mut evidence = false;
        let mut after: Option<Kind> = None; // Chain or Redirect just seen
        while let Some((kind, end)) = next_arg(t, i) {
            if kind == Kind::Ws { i = end; continue; }
            // the closing backtick of inline code ends the command
            if cmd_in_code && !in_code(i, end) { break; }
            let tok = &text[i..end];
            // `--out=dist`: the word glued to a flag is its value, not a subcommand
            if after_eq && matches!(kind, Kind::Sub | Kind::Word) {
                after_eq = false;
                out.push((i, end, Color::Path));
                nargs += 1;
                i = end;
                continue;
            }
            after_eq = kind == Kind::Flag && tok.ends_with('=');
            match after {
                // `&& git ...`: only a new command may follow
                Some(Kind::Chain) if !matches!(kind, Kind::Chain | Kind::Redirect) => {
                    if matches!(kind, Kind::Sub | Kind::Word) && v.commands.contains(tok) {
                        out.push((i, end, Color::Cmd));
                        cmd = tok; subs = 0; after = None; evidence = true;
                        i = end;
                        continue;
                    }
                    // `&& ./run.sh`, `| $PAGER`: a script or variable runs next
                    if kind == Kind::Path && (tok.starts_with("./") || tok.starts_with('/') || tok.starts_with('~'))
                        || kind == Kind::Var
                    {
                        out.push((i, end, if kind == Kind::Var { Color::Var } else { Color::Path }));
                        cmd = ""; subs = 0; after = None; evidence = true; nargs += 1;
                        i = end;
                        continue;
                    }
                    break;
                }
                // `> out.txt`, `<< 'EOF'`: a target, then back to normal
                Some(Kind::Redirect) if !matches!(kind, Kind::Chain | Kind::Redirect) => {
                    let color = match kind {
                        Kind::Str => Color::Str, Kind::Var => Color::Var, Kind::Num => Color::Num,
                        Kind::Comment | Kind::Flag => { break; }
                        _ => Color::Path,
                    };
                    out.push((i, end, color));
                    after = None; nargs += 1; evidence = true;
                    i = end;
                    continue;
                }
                _ => {}
            }
            let color = match kind {
                Kind::Sub | Kind::Word => {
                    // `sudo systemctl restart ...`: the runner hands off to a
                    // real command, so restart the highlight from there
                    if v.chain_tools.contains(cmd) && v.commands.contains(tok) {
                        out.push((i, end, Color::Cmd));
                        cmd = tok; subs = 0; nargs += 1; evidence = true;
                        i = end;
                        continue;
                    }
                    if v.stop_words.contains(tok) { break; }
                    let sub_ok = kind == Kind::Sub && v.subcmd_tools.contains(cmd) && subs < MAX_SUB;
                    if sub_ok { subs += 1; Color::Sub }
                    else if strong || in_code(i, end) { Color::Path }
                    else {
                        // prose: take a few bare words on trust; if no flag,
                        // path or operator ever shows up the span is dropped
                        bare += 1;
                        if bare > MAX_BARE { break; }
                        Color::Path
                    }
                }
                Kind::Flag => Color::Flag,
                Kind::Str => Color::Str,
                Kind::Path => Color::Path,
                Kind::Num => Color::Num,
                Kind::Var => Color::Var,
                Kind::Url => Color::Url,
                Kind::Comment => Color::Comment,
                Kind::Chain | Kind::Redirect => { after = Some(kind); Color::Op }
                Kind::Ws => unreachable!(),
            };
            // operators only count once something real follows them
            if !matches!(kind, Kind::Sub | Kind::Word | Kind::Chain | Kind::Redirect) { evidence = true; }
            // a bare word followed by sentence punctuation ends the span
            if matches!(kind, Kind::Sub | Kind::Word | Kind::Path | Kind::Num | Kind::Var) && tok.len() > 1
                && is_sentence_punct(t[end - 1])
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
        // a trailing chain operator paints nothing on its own
        if after == Some(Kind::Chain) || after == Some(Kind::Redirect) {
            if let Some(last) = out.last() { if last.2 == Color::Op { i = last.0; out.pop(); nargs -= 1; } }
        }
        if nargs == 0 || (!strong && !evidence) {
            // bare command word in prose, or `make sure`: leave it alone
            out.truncate(mark);
            pos = ce;
            continue;
        }
        pos = i;
    }
}

// ---- attributes ------------------------------------------------------------

#[derive(Clone, PartialEq, Default, Debug)]
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

    /// One SGR that reproduces this attribute set from scratch. `fg_override`
    /// is SGR params (e.g. `1;38;2;r;g;b`) that replace the cell's own fg.
    fn render(&self, fg_override: &str) -> String {
        let mut s = String::from("\x1b[0");
        if self.bold { s.push_str(";1") }
        if self.dim { s.push_str(";2") }
        if self.italic { s.push_str(";3") }
        if self.underline { s.push_str(";4") }
        if self.blink { s.push_str(";5") }
        if self.inverse { s.push_str(";7") }
        if self.hidden { s.push_str(";8") }
        if self.strike { s.push_str(";9") }
        if !self.bg.is_empty() { s.push(';'); s.push_str(&self.bg) }
        if !fg_override.is_empty() { s.push(';'); s.push_str(fg_override) }
        else if !self.fg.is_empty() { s.push(';'); s.push_str(&self.fg) }
        s.push('m');
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
            if self.autowrap { self.col = 0; self.linefeed(); } else { self.col = self.cols.saturating_sub(w); }
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
                    3 => {} // erase scrollback only: the visible screen is untouched
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

    /// Text of one row plus, per byte, the cell it came from and whether that
    /// cell is inline code. `cell_of` has one extra entry mapping `text.len()`
    /// to `cols`, so span ends can be looked up too.
    fn row_text(&self, r: usize, text: &mut String, cell_of: &mut Vec<usize>, code: &mut Vec<bool>) {
        text.clear(); cell_of.clear(); code.clear();
        let cs = codespan_fg();
        for (ci, cell) in self.grid[r].iter().enumerate() {
            if cell.cont { continue; }
            let start = text.len();
            text.push(cell.ch);
            if let Some(z) = &cell.zw { text.push_str(z); }
            let is_code = cell.attr.fg == cs;
            for _ in start..text.len() { cell_of.push(ci); code.push(is_code); }
        }
        cell_of.push(self.cols);
    }

    /// Colour code wanted for every cell of row `r`: remaps first, then the
    /// tokenizer's spans on top, then wide-char continuations follow their head.
    fn desired_row(&mut self, r: usize, desired: &mut Vec<u8>, text: &mut String, cell_of: &mut Vec<usize>, code: &mut Vec<bool>) {
        self.row_text(r, text, cell_of, code);
        self.spans_buf.clear();
        spans(text, code, &mut self.spans_buf);
        desired.clear(); desired.resize(self.cols, 0);
        let rm = remaps();
        if !rm.is_empty() {
            for (ci, cell) in self.grid[r].iter().enumerate() {
                if let Some(k) = rm.iter().position(|(from, _)| *from == cell.attr.fg) { desired[ci] = REMAP_BASE + k as u8; }
            }
        }
        for &(s, e, color) in &self.spans_buf {
            let (cs, ce) = (cell_of[s], cell_of[e]);
            for d in desired.iter_mut().take(ce).skip(cs) { *d = color as u8; }
        }
        for c in 1..self.cols { if self.grid[r][c].cont { desired[c] = desired[c - 1]; } }
    }

    /// Emit repaint escapes for dirty rows. Returns bytes to append to stdout.
    fn repaint(&mut self, out: &mut Vec<u8>) {
        if (!self.enabled && !self.alt) || self.pending_wrap { return; }
        // a read boundary can fall inside an escape sequence or a multi-byte
        // char; injecting bytes there would corrupt it. Rows stay dirty and
        // paint on the next chunk.
        if self.pst != PState::Ground || !self.utf8.is_empty() { return; }
        let (mut text, mut cell_of, mut code) = (String::new(), Vec::new(), Vec::new());
        let mut desired: Vec<u8> = Vec::new();
        let mut wrote = false;
        // unchanged cells between two runs cost less to rewrite than a
        // cursor move plus a fresh SGR, so short gaps join the run
        const GAP: usize = 3;
        for r in 0..self.rows {
            if !self.dirty[r] { continue; }
            self.dirty[r] = false;
            self.desired_row(r, &mut desired, &mut text, &mut cell_of, &mut code);
            let stale = |c: usize, row: &[Cell]| desired[c] != row[c].shown || row[c].cont;
            let mut c = 0;
            while c < self.cols {
                if !stale(c, &self.grid[r]) { c += 1; continue; }
                let start = c;
                let mut last_attr: Option<(Rc<Attr>, u8)> = None;
                let mut seg = String::new();
                loop {
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
                    if c >= self.cols { break; }
                    if stale(c, &self.grid[r]) { continue; }
                    let more = (c + 1..(c + 1 + GAP).min(self.cols)).any(|k| stale(k, &self.grid[r]));
                    if !more { break; }
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
    fn render_inline(&mut self) -> String {
        let mut s = String::new();
        let (mut text, mut cell_of, mut code) = (String::new(), Vec::new(), Vec::new());
        let mut desired: Vec<u8> = Vec::new();
        let last_row = (0..self.rows).rev().find(|&r| self.grid[r].iter().any(|c| c.ch != ' ')).map_or(0, |r| r + 1);
        for r in 0..last_row {
            self.desired_row(r, &mut desired, &mut text, &mut cell_of, &mut code);
            let end = self.grid[r].iter().rposition(|c| c.ch != ' ').map_or(0, |i| i + 1);
            let mut last: Option<(Rc<Attr>, u8)> = None;
            for c in 0..end {
                let cell = &self.grid[r][c];
                if cell.cont { continue; }
                let need = match &last { Some((a, col)) => **a != *cell.attr || *col != desired[c], None => true };
                if need { s.push_str(&cell.attr.render(code_sgr(desired[c]))); last = Some((cell.attr.clone(), desired[c])); }
                s.push(cell.ch);
                if let Some(z) = &cell.zw { s.push_str(z); }
            }
            s.push_str("\x1b[0m\n");
        }
        s
    }

    #[cfg(test)]
    fn row_string(&self, r: usize) -> String {
        self.grid[r].iter().filter(|c| !c.cont).map(|c| c.ch).collect::<String>().trim_end().to_string()
    }
}

// ---- PTY plumbing ----------------------------------------------------------

static WINCH: AtomicBool = AtomicBool::new(false);
/// SIGTERM/SIGHUP received: leave the loop, restore the terminal, pass it on
static QUIT: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_winch(_: libc::c_int) { WINCH.store(true, Ordering::SeqCst); }
extern "C" fn on_quit(sig: libc::c_int) { QUIT.store(sig, Ordering::SeqCst); }

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
        unsafe {
            libc::execvp(ptrs[0], ptrs.as_ptr());
            // only write(2) is safe here; the message goes to the pty, which
            // the parent passes through to the terminal
            let msg = format!("claude-hl: cannot run {:?}: {}\r\n", argv[0], std::io::Error::last_os_error());
            libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const _, msg.len());
            libc::_exit(127);
        }
    }
    if isatty {
        unsafe { libc::signal(libc::SIGWINCH, on_winch as extern "C" fn(libc::c_int) as libc::sighandler_t); }
    }
    unsafe {
        let h = on_quit as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGHUP, h);
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
        let q = QUIT.load(Ordering::SeqCst);
        if q != 0 { unsafe { libc::kill(pid, q); } break; }
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
    if libc::WIFSIGNALED(status) { return 128 + libc::WTERMSIG(status); }
    1
}

// ---- entry -----------------------------------------------------------------

const SAMPLE: &str = "Ran git diff --stat && git status --short\r\n\
Ran git diff -- crates/ts_checker/src/semantic/assignment.rs | sed -n '1,300p'\r\n\
Ran cargo test --release -- --nocapture 2>&1 | tail -n 5\r\n\
Ran git commit -m \"release: v1.2.0\" --no-verify && git tag v1.2.0\r\n\
Run git status to see changes, then:\r\n\
  git push origin HEAD --force-with-lease=main   # after rebase\r\n\
  npm install --save-dev vitest and restart the server.\r\n\
  sudo systemctl restart nginx && journalctl -u nginx --since today\r\n\
  pytest tests/test_auth.py -k \"login\" | tail -20 > /tmp/out.log\r\n\
  docker run -it --rm -v $(pwd):/app -e PORT=3000 node:20 bash\r\n\
  git clone https://github.com/rashedInt32/claude-hl && cd claude-hl && chmod +x build.sh\r\n\
Use \x1b[38;2;95;179;217mclaude --rc \"my-project\"\x1b[39m from the project dir.\r\n\
Tagged. \x1b[38;2;177;185;249mv1.2.0\x1b[39m is on \x1b[38;2;177;185;249mmain\x1b[39m; push with \x1b[38;2;177;185;249mgit push --follow-tags\x1b[39m when ready.\r\n\
Let me make sure the build passes, then go ahead with the next step; run \x1b[38;2;177;185;249mnpm test\x1b[39m after.\r\n\
Plain prose with the word node in it, and cd ~/Documents/codes/packages.\r\n\
\x1b[1m● streamed:\x1b[22m Ran git\x1b[0m";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("claude-hl {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.first().map(String::as_str) == Some("--themes") {
        // palette() is fixed per process, so preview each theme in a child
        let exe = std::env::current_exe().unwrap_or_else(|_| "claude-hl".into());
        for name in THEME_NAMES {
            println!("\x1b[1m{name}\x1b[0m  (CLAUDE_HL_THEME={name})");
            let _ = std::process::Command::new(&exe).arg("--selftest").env("CLAUDE_HL_THEME", name).status();
            println!();
        }
        return;
    }
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

    /// `[tok:Kind]` markup of what `spans` would paint, e.g. `[git:Cmd] [status:Sub]`.
    fn paint_with(line: &str, code: &[bool]) -> String {
        let mut v = Vec::new();
        spans(line, code, &mut v);
        let mut o = String::new();
        let mut last = 0;
        for (a, b, c) in v {
            o.push_str(&line[last..a]);
            o.push_str(&format!("[{}:{:?}]", &line[a..b], c));
            last = b;
        }
        o.push_str(&line[last..]);
        o
    }
    fn paint(line: &str) -> String { paint_with(line, &vec![false; line.len()]) }
    /// like `paint`, but bytes in `code_range` count as inline code
    fn paint_code(line: &str, code_range: std::ops::Range<usize>) -> String {
        let mut code = vec![false; line.len()];
        for b in code.iter_mut().take(code_range.end).skip(code_range.start) { *b = true; }
        paint_with(line, &code)
    }

    #[test]
    fn prose_with_tool_names_stays_plain() {
        for line in [
            "Let me make sure the build passes.",
            "I'll go ahead and fix that, then go through the rest.",
            "The next step is to check the bundle size.",
            "The next step; run it again.",
            "Plain prose with the word node in it.",
            "We should open the file and cat images later.",
            "git status",
            "run npm test to check",
            "git push origin HEAD",
        ] {
            assert_eq!(paint(line), line, "should stay plain: {line}");
        }
    }

    #[test]
    fn runner_prefix_marks_a_command_line() {
        assert_eq!(paint("Ran git status --short"), "Ran [git:Cmd] [status:Sub] [--short:Flag]");
        assert_eq!(paint("Ran git status"), "Ran [git:Cmd] [status:Sub]");
        assert_eq!(paint("Run git status to see changes"), "Run [git:Cmd] [status:Sub] to see changes");
        assert_eq!(paint("$ git push origin HEAD"), "$ [git:Cmd] [push:Sub] [origin:Sub] [HEAD:Path]");
    }

    #[test]
    fn inline_code_is_trusted_and_ends_at_the_backtick() {
        // "run npm test after." with `npm test` drawn as inline code
        let line = "run npm test after.";
        assert_eq!(paint_code(line, 4..12), "run [npm:Cmd] [test:Sub] after.");
        let line = "push with git push --follow-tags when ready.";
        assert_eq!(paint_code(line, 10..32), "push with [git:Cmd] [push:Sub] [--follow-tags:Flag] when ready.");
    }

    #[test]
    fn prose_needs_evidence_but_gets_it_late() {
        assert_eq!(paint("git push origin HEAD --force-with-lease=main   # after rebase"),
            "[git:Cmd] [push:Sub] [origin:Sub] [HEAD:Path] [--force-with-lease=:Flag][main:Path]   [# after rebase:Comment]");
        assert_eq!(paint("npm install --save-dev vitest and restart the server."),
            "[npm:Cmd] [install:Sub] [--save-dev:Flag] [vitest:Sub] and restart the server.");
        assert_eq!(paint("and cd ~/Documents/codes."), "and [cd:Cmd] [~/Documents/codes:Path].");
    }

    #[test]
    fn operators_chain_and_redirect() {
        assert_eq!(paint("sudo systemctl restart nginx && journalctl -u nginx"),
            "[sudo:Cmd] [systemctl:Cmd] [restart:Sub] [nginx:Sub] [&&:Op] [journalctl:Cmd] [-u:Flag] [nginx:Sub]");
        assert_eq!(paint("tail -20 > /tmp/out.log"), "[tail:Cmd] [-20:Flag] [>:Op] [/tmp/out.log:Path]");
        assert_eq!(paint("cat <<'EOF' > x.txt"), "[cat:Cmd] [<<:Op]['EOF':Str] [>:Op] [x.txt:Path]");
        assert_eq!(paint("make build && ./run.sh"), "[make:Cmd] [build:Sub] [&&:Op] [./run.sh:Path]");
        assert_eq!(paint("cargo test 2>&1 | tail -n 5"), "[cargo:Cmd] [test:Sub] [2>&1:Op] [|:Op] [tail:Cmd] [-n:Flag] [5:Num]");
        // a trailing operator (streamed line, or prose) paints nothing on its own
        assert_eq!(paint("Ran git status &&"), "Ran [git:Cmd] [status:Sub] &&");
        assert_eq!(paint("time cargo build --release"), "[time:Cmd] [cargo:Cmd] [build:Sub] [--release:Flag]");
    }

    #[test]
    fn token_kinds() {
        assert_eq!(paint("docker run -e PORT=3000 -v $(pwd):/app node:20"),
            "[docker:Cmd] [run:Sub] [-e:Flag] [PORT=3000:Var] [-v:Flag] [$(pwd):/app:Var] [node:20:Path]");
        assert_eq!(paint("cd $HOME/.config"), "[cd:Cmd] [$HOME/.config:Var]");
        assert_eq!(paint("git clone https://github.com/x/y.git"), "[git:Cmd] [clone:Sub] [https://github.com/x/y.git:Url]");
        assert_eq!(paint("git tag v1.2.0 && sleep 10s"), "[git:Cmd] [tag:Sub] [v1.2.0:Num] [&&:Op] [sleep:Cmd] [10s:Num]");
        assert_eq!(paint("chmod +x build.sh"), "[chmod:Cmd] [+x:Flag] [build.sh:Path]");
        assert_eq!(paint("cargo test -- --nocapture"), "[cargo:Cmd] [test:Sub] [--:Flag] [--nocapture:Flag]");
        assert_eq!(paint("Ran cd -"), "Ran [cd:Cmd] [-:Flag]");
        assert_eq!(paint("Ran ls *.rs"), "Ran [ls:Cmd] [*.rs:Path]");
        assert_eq!(paint("Ran cargo build 2>/dev/null"), "Ran [cargo:Cmd] [build:Sub] [2>:Op][/dev/null:Path]");
        assert_eq!(paint("Ran grep foo|wc -l"), "Ran [grep:Cmd] [foo:Path][|:Op][wc:Cmd] [-l:Flag]");
        assert_eq!(paint("git commit -m \"fix: it\" --no-verify"), "[git:Cmd] [commit:Sub] [-m:Flag] [\"fix: it\":Str] [--no-verify:Flag]");
    }

    #[test]
    fn sentence_punctuation_ends_the_span() {
        assert_eq!(paint("Ran tail -n 5."), "Ran [tail:Cmd] [-n:Flag] [5:Num].");
        assert_eq!(paint("Ran git push origin main, then wait."), "Ran [git:Cmd] [push:Sub] [origin:Sub] [main:Sub], then wait.");
    }

    fn screen(rows: usize, cols: usize, input: &str) -> Screen {
        let mut sc = Screen::new(rows, cols);
        sc.feed(input.as_bytes());
        sc
    }

    #[test]
    fn screen_writes_wraps_and_scrolls() {
        let sc = screen(3, 5, "abc\r\ndefghij");
        assert_eq!(sc.row_string(0), "abc");
        assert_eq!(sc.row_string(1), "defgh");
        assert_eq!(sc.row_string(2), "ij");
        assert_eq!((sc.row, sc.col), (2, 2));
        let sc = screen(2, 10, "one\r\ntwo\r\nthree");
        assert_eq!((sc.row_string(0), sc.row_string(1)), ("two".into(), "three".into()));
    }

    #[test]
    fn screen_cursor_and_erase() {
        let mut sc = screen(3, 10, "hello world\x1b[1;7H\x1b[K");
        assert_eq!(sc.row_string(0), "hello");
        sc.feed(b"\x1b[3J"); // scrollback only: screen untouched
        assert_eq!(sc.row_string(0), "hello");
        sc.feed(b"\x1b[2J");
        assert_eq!(sc.row_string(0), "");
    }

    #[test]
    fn screen_scroll_region() {
        let mut sc = screen(4, 10, "a\r\nb\r\nc\r\nd");
        sc.feed(b"\x1b[2;3r\x1b[3;1H\n"); // region rows 2-3, cursor on row 3, LF scrolls the region
        assert_eq!([sc.row_string(0), sc.row_string(1), sc.row_string(2), sc.row_string(3)], ["a", "c", "", "d"]);
    }

    #[test]
    fn screen_alt_screen_round_trip() {
        let mut sc = screen(2, 10, "main");
        sc.feed(b"\x1b[?1049h");
        assert_eq!(sc.row_string(0), "");
        sc.feed(b"alt\x1b[?1049l");
        assert_eq!(sc.row_string(0), "main");
        assert!(!sc.alt);
    }

    #[test]
    fn screen_wide_chars() {
        let sc = screen(1, 6, "日本x");
        assert!(sc.grid[0][1].cont && sc.grid[0][3].cont);
        assert_eq!(sc.row_string(0), "日本x");
        assert_eq!(sc.col, 5);
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('日'), 2);
        assert_eq!(char_width('\u{301}'), 0);
    }

    #[test]
    fn repaint_paints_once_and_restores_the_cursor() {
        let mut sc = screen(3, 40, "Ran git status --short\r\n");
        let mut out = Vec::new();
        sc.repaint(&mut out);
        let s = String::from_utf8(out.clone()).unwrap();
        assert!(s.starts_with("\x1b[1;5H"), "paint starts at the command: {s:?}");
        assert!(s.contains("git") && s.contains("status") && s.contains("--short"));
        // tokens a space apart share one run: one move to paint, one to restore
        assert_eq!(s.matches('H').count(), 2, "{s:?}");
        assert!(s.ends_with("\x1b[2;1H\x1b[0m"), "cursor restored: {s:?}");
        out.clear();
        sc.repaint(&mut out);
        assert!(out.is_empty(), "nothing left to paint");
    }

    #[test]
    fn repaint_follows_a_streamed_line() {
        let mut sc = screen(2, 40, "Ran git");
        let mut out = Vec::new();
        sc.repaint(&mut out);
        assert!(out.is_empty(), "bare command word: nothing yet");
        sc.feed(b" status");
        sc.repaint(&mut out);
        assert!(String::from_utf8(out).unwrap().contains("status"));
    }

    #[test]
    fn repaint_waits_for_a_complete_escape() {
        let mut sc = screen(2, 40, "Ran git status\x1b[");
        let mut out = Vec::new();
        sc.repaint(&mut out);
        assert!(out.is_empty());
        sc.feed(b"0m");
        sc.repaint(&mut out);
        assert!(!out.is_empty());
    }

    #[test]
    fn remapped_foreground_is_wanted_even_in_prose() {
        let mut sc = screen(1, 20, "\x1b[38;2;177;185;249mfoo\x1b[39m bar");
        let (mut d, mut t, mut c, mut k) = (Vec::new(), String::new(), Vec::new(), Vec::new());
        sc.desired_row(0, &mut d, &mut t, &mut c, &mut k);
        assert_eq!(d[0], REMAP_BASE);
        assert_eq!(d[4], 0);
        assert_eq!(k[0..3], [true, true, true]);
    }

    #[test]
    fn attr_apply_and_render() {
        let mut a = Attr::default();
        a.apply(&[1, 38, 2, 10, 20, 30, 48, 5, 7]);
        assert!(a.bold);
        assert_eq!(a.fg, "38;2;10;20;30");
        assert_eq!(a.bg, "48;5;7");
        assert_eq!(a.render(""), "\x1b[0;1;48;5;7;38;2;10;20;30m");
        assert_eq!(a.render("38;2;1;2;3"), "\x1b[0;1;48;5;7;38;2;1;2;3m");
        a.apply(&[]);
        assert_eq!(a, Attr::default());
    }
}
