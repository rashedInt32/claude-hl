# claude-hl

Run a TUI (default: `claude`) inside a PTY and paint shell commands in its
output, Codex-style. The wrapped program runs completely unchanged, so hooks,
skills, MCP, permissions and `/rc` all survive. Only `libc` as a dependency.

```
cargo build --release && cp target/release/claude-hl ~/.local/bin/
claude-hl [args passed to claude...]
CLAUDE_HL_CMD=codex claude-hl        # wrap something else
CLAUDE_HL_THEME=rose claude-hl       # rose-pine palette (default: codex)
claude-hl --selftest                 # print sample highlighted text
CLAUDE_HL_REMAP=b1b9f9=d2d6ff claude-hl   # add/override foreground recolours; empty disables
```

How it works: the child's raw output passes through untouched while a small
VT emulator mirrors the screen (cursor, cell grid, attributes). After each
chunk, rows whose text changed are re-tokenised and only the cells whose
colour should differ are repainted with absolute cursor moves; cursor and
attributes are then restored. This survives renderers that stream a line in
pieces, which Claude Code does. At startup the terminal is asked for the
cursor position (DSR); if it does not answer, highlighting is disabled and
the stream is passed through unchanged. Width is never changed.

Token classes: command, subcommand, flag, quoted string, operator
(`&& || | ; > >> < 2>&1`), path/number. `CLAUDE_HL_DUMP=/path` appends the
raw PTY stream to a file for debugging.

Foreground remaps: every cell the app drew with an exact truecolor
foreground is shown in another colour. Claude Code's markdown renderer
resolves the theme by name and ignores custom-theme overrides for inline
code, so both themes remap its stock lavender `b1b9f9` by default (codex →
`c5cbff`, rose → `c4a7e7`). `CLAUDE_HL_REMAP` takes comma-separated `from=to`
hex pairs that add to or override the defaults; set it empty to disable.

It reads rendered ANSI, not markdown, so it guesses commands with a
vocabulary and will occasionally paint prose. That is inherent.
