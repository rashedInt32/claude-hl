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

One dependency (`libc`), one 380 KB binary. Built and used on macOS; Linux should work but hasn't been tried.

## Use

```sh
claude-hl                  # instead of `claude`; any args pass straight through
claude-hl --resume abc123
```

That's it. There's nothing to configure.

## Tweak

| Variable | What it does |
|---|---|
| `CLAUDE_HL_THEME=rose` | Rose Pine palette instead of the default Codex-style one |
| `CLAUDE_HL_CMD=codex` | Wrap a different program |
| `CLAUDE_HL_REMAP=b1b9f9=a99cff` | Recolour any exact foreground the app draws. Comma-separate pairs; empty disables |
| `CLAUDE_HL_DUMP=/tmp/hl.bin` | Append the raw PTY stream to a file, for bug reports |

`claude-hl --selftest` prints a sample so you can check colours without starting Claude.

### Why is there a remap at all?

Claude Code's inline code (`like this`) always uses the stock lavender, even
with a custom theme. The markdown renderer looks the theme up by name and never
sees your overrides. claude-hl already knows every cell's colour, so it swaps
that lavender for something that fits each theme. Set your own pair if you
disagree with the pick.

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

Tokens it knows: command, subcommand, flag, quoted string, operator
(`&& || | ; > >> < 2>&1`), path, number. A stop-word list and a per-tool
subcommand allowlist keep prose like "node here" unpainted.

## Limits

It reads rendered ANSI, not markdown. It can't know a fence's language, and it
recognises commands by vocabulary, so now and then a prose word gets painted.
That's inherent to the approach. If a real command is missed, add it to
`COMMANDS` in `src/main.rs` and rebuild.

## License

MIT
