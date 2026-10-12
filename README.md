# claude-hl

Syntax colours for shell commands in Claude Code's output. Like Codex does it.

![The same Claude Code session, plain on top and painted by claude-hl below](docs/before-after.png)

Claude Code shows inline commands in one flat colour. `git commit -m "fix" --no-verify`
is just a string. claude-hl sits between Claude Code and your terminal and paints
the command blue, the flags pink, the string gold. Claude Code itself runs
unchanged, so hooks, skills, MCP, permissions and `/rc` all keep working.

## Install

Needs a Rust toolchain ([rustup.rs](https://rustup.rs)).

```sh
cargo install --git https://github.com/rashedInt32/claude-hl
```

That drops `claude-hl` into `~/.cargo/bin`, which rustup already put on your
PATH. Or clone and build it yourself:

```sh
git clone https://github.com/rashedInt32/claude-hl
cd claude-hl
cargo build --release
cp target/release/claude-hl ~/.local/bin/
```

One dependency (`libc`), one 411 KB binary. Built and used on macOS; Linux should work but hasn't been tried.

## Use

```sh
claude-hl                  # instead of `claude`; any args pass straight through
claude-hl --resume abc123
```

That's it. There's nothing to configure.

## Tweak

| Variable | What it does |
|---|---|
| `CLAUDE_HL_THEME=rose` | Pick a palette: `codex` (default), `rose`, `catppuccin`, `tokyonight`, `dracula`, `gruvbox`, `nord` |
| `CLAUDE_HL_COLORS=cmd=89b4fa,num=fab387` | Override single slots of the theme. Slots: `cmd sub flag string path op num var url comment tool err warn ok` |
| `CLAUDE_HL_CODE_BG=2a2a3a` | Draw a background behind inline code, GitHub style. Off by default |
| `CLAUDE_HL_COMMANDS="bash sh -make"` | Grow the vocabulary without a rebuild. `word` adds a command, `word:sub` adds one that takes subcommands (`just:sub`), `-word` removes one |
| `CLAUDE_HL_PRIVATE=435872` | One colour for Claude's private notes, instead of half the text colour |
| `CLAUDE_HL_BOTTOM_LINE=head=5eead4,…` | Colour a `Bottom line` summary block. The slots are `head`, `verified`, `issue`, `fix` and `text`, as in `head=5eead4,verified=bef264,issue=ff9e8a,fix=f0abfc,text=d6deeb`: the heading and each `Verified:`, `Issue:` and `Fix:` label in bold, and the plain text under them. Leave a slot out to keep that part as drawn; one bare `rrggbb` colours the heading and all labels. Off by default |
| `CLAUDE_HL_MARK=1` | Draw a `▎` in the left gutter beside each paragraph of Claude's prose that asks something of you: a question, an instruction, a risk, or a thing it did not do. `1` uses the theme's warn colour; `rrggbb` picks one. Off by default |
| `CLAUDE_HL_FG=94a4b6` | Your terminal's text colour, so private notes can be drawn at half of it. Only needed where the terminal won't report it, such as inside tmux |
| `CLAUDE_HL_CMD=codex` | Wrap a different program |
| `CLAUDE_HL_REMAP=b1b9f9=a99cff` | Recolour any exact foreground the app draws. Comma-separate pairs; empty disables |
| `CLAUDE_HL_DUMP=/tmp/hl.bin` | Append the raw PTY stream to a file, for bug reports |

`claude-hl --selftest` prints a sample so you can check colours without starting
Claude. `claude-hl --themes` prints that sample once per theme, so you can pick
one by eye. `claude-hl --version` prints the wrapper's own version; every other
argument goes to Claude.

The screenshot at the top is the default `codex` theme. Here is
[the same session in `tokyonight`](docs/before-after-tokyonight.png).

### Why is there a remap at all?

Claude Code's inline code (`like this`) always uses the stock lavender, even
with a custom theme. The markdown renderer looks the theme up by name and never
sees your overrides. claude-hl already knows every cell's colour, so it swaps
that lavender for something that fits each theme. Set your own pair if you
disagree with the pick.

### Why is a paragraph faint?

Claude Code sometimes prints its own planning note, a paragraph that opens with
`Private` or `Privately` and sits between blank rows. claude-hl draws that
whole paragraph in italic at half brightness, so the real answer stands out.
`CLAUDE_HL_PRIVATE` swaps half brightness for a colour of your choice. It asks your
terminal for its text colour to do that. tmux won't say, so set
`CLAUDE_HL_FG` there; without it you get the terminal's own faint style.

### Colouring a Bottom line summary

If your instructions have Claude open replies with a `Bottom line` heading and
`Verified:`, `Issue:` and `Fix:` lines under it, `CLAUDE_HL_BOTTOM_LINE` paints
that block down to the next blank row. The heading and each label turn bold in
their own colour and the sentences take `text`, so each line is easy to find.
Commands, paths and inline code inside keep their usual colours.

### Marking what needs you

With `CLAUDE_HL_MARK` set, a `▎` appears beside each paragraph of a reply that
asks something of you: a question, an instruction, a risk, or something Claude
did not do. Those are matters of wording, so a small table of phrases decides
them in microseconds.

Marks need Claude's text as written, not as it wrapped on screen, so with
`CLAUDE_HL_MARK` set claude-hl starts Claude with a session-only plugin. Its
`MessageDisplay` hook is `claude-hl --hook`: Claude Code runs it with each
batch of lines just before drawing them, and the hook relays the batch to the
wrapper over a socket in a private temp dir, then exits. That costs about 5 ms
per paragraph and shows nothing. The paragraph is labelled from the markdown,
so bold labels such as `**Body:**` and fenced code count, and matched to its
rows on screen when they appear. A reply to "write me a post" is left whole,
since the reply is the thing you asked for, and list items, code, headings and
tool output are never marked. The plugin is added with `--plugin-dir`, so your
own hooks and `--settings` still apply. Nothing is written outside the temp
dir, and it is removed on exit.

Nothing is ever dimmed. An earlier version dimmed whatever it had not marked,
which greyed out whole answers, and a version after that asked a judgement
model per reply. Both are gone: what a paragraph asks of you is a matter of
wording and rules read it well, while what is worth reading is not, and no
approach tried here earned the network call.

## Why not a custom frontend?

There are good ones. [claude-code-rust](https://github.com/srothgan/claude-code-rust)
is a Ratatui TUI over the Agent SDK with real syntax highlighting on real
markdown, and [toad](https://github.com/batrachianai/toad) does similar over
ACP. Editor integrations like CodeCompanion and Sidekick render Claude's
output inside a buffer where the editor's own highlighter takes over.

All of them replace the Claude CLI. That's the part I didn't want to give up.
The CLI is where hooks, skills, plugins, MCP servers, permission prompts, plan
mode, `/resume`, `/plugin`, Remote Control and every new feature land first. A
frontend has to re-implement each of those or live without it, and it's
always a release behind.

claude-hl is the other trade. It gives up knowing the markdown (it only sees
rendered ANSI) in exchange for changing nothing else. Claude Code runs exactly
as shipped; the wrapper just recolours what's already on screen. If one day
Claude Code colours inline commands itself, delete the binary and nothing
else changes.

## How it works

The child's output passes through byte for byte. Alongside, a small terminal
emulator mirrors the screen: cursor, cells, attributes, scroll regions, the
alternate screen. After each chunk, rows whose text changed are re-tokenised
and only the cells whose colour should differ get rewritten with an absolute
cursor move. Then the cursor and attributes are put back.

That design is what makes it stable. Claude Code streams a line in fragments
(`Ran git`, then ` status`, then ` --short`), and a byte-level filter can't
colour a fragment it can't see the start of. A screen model can.

Tokens it knows: command (bold), subcommand, flag (`--long`, `-s`, `-20`,
`+x`, `--key=` with its value painted separately), quoted string, operator
(`&& || | ; > >> < << 2>&1`), path and glob, number and version (`5`, `v1.2.0`,
`10s`), variable (`$HOME`, `$(pwd)`, `PORT=3000`), URL (underlined) and a
trailing `# comment`. Prefix runners chain: in `sudo systemctl restart nginx`
both words paint as commands, and so do `time`, `watch`, `env`, `xargs`.

Prose is the hard part. `make sure`, `go ahead` and `next step` are all valid
command shapes. The wrapper uses two signals a byte filter never sees. Claude
Code draws inline code in one fixed colour, so a command inside backticks is
trusted completely and the highlight stops where the code span stops. Tool
recaps start with `Ran`, so a command after that prefix is trusted too. Anywhere
else, a command only paints once a flag, path, string, number or operator turns
up; `git push --follow-tags` in a sentence paints, `git status` in a sentence
does not, and neither does `make sure the build passes`.

Beyond commands, a second pass paints what the first left alone:

- **Paths and URLs anywhere.** `target/release/build.log`, `~/.config/app.toml`,
  `README.md`, `.gitignore`, `https://docs.rs/libc`. A `src/main.rs:42:7`
  reference gets its line number in the number colour. Bare words need a known
  extension or a leading `/`, `./` or `~/`, so "and/or" and "e.g." stay plain.
- **Tool output.** Claude Code draws tool results in one flat gray. Inside that
  gray only, `error`, `FAILED` and `panicked` go red, `warning` yellow, `ok`,
  `passed` and `Done` green, numbers and durations (`23`, `0.42s`, `1.2k`) get
  the number colour, and `git status --short` codes paint by kind. The same
  words in Claude's prose are left alone.
- **Tool lines.** `⏺ Read(src/main.rs)`, `⏺ Bash(cargo test)`: the tool name
  in its own colour, the argument as a path or a command.
- **Chrome.** Box drawing and the `⎿` connector are dimmed when the app drew
  them in the default colour.

Repaints are cheap on the terminal side: tokens a few cells apart share one
cursor move and one SGR, and attributes are emitted as a single sequence.

## Limits

It reads rendered ANSI, not markdown. It can't know a fence's language, and it
recognises commands by vocabulary, so now and then a prose word gets painted,
or an unbackticked `git status` in a sentence stays plain. That's inherent to
the approach. If a real command is missed, add it with `CLAUDE_HL_COMMANDS`,
or to `COMMANDS` in `src/main.rs`. `cargo test` covers the tokenizer and the
screen model, so a vocabulary change is easy to check.

The palettes assume a dark background. Claude Code's light themes have no
matching palette yet.

## License

MIT
