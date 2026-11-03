# lama

Terminal UI that visualizes Claude Code and Codex agent sessions as a live flow graph. The same core also runs in the browser, compiled to wasm. Read-only and zero network, with one deliberate exception, both through kitty remote control (`src/kitty.rs`), or through tmux (`src/tmux.rs`) for sessions in a tmux pane: the session console (`src/console.rs`) types prompts into an agent's own kitty window and answers its permission dialogs (read off its screen by `src/ask.rs`), stops a running turn (Esc) or resumes a closed session in a new kitty tab, and the composer (`src/composer.rs`) starts a new agent session in a new kitty tab. Separately, `src/clipboard.rs` copies text selected in a console to the clipboard (OSC 52) and reads it back for pasting, `src/news.rs` (only with `LAMA_NEWS=on`, lama's one use of the network) fetches Hacker News and Lobsters front pages with `curl` and has a tool-less `claude -p` on Haiku write them up into `~/.local/share/lama/news/`, and tasks (`src/task/`) go further: on your request lama runs a conductor agent in the background (headless `claude -p` with read-only tools, or `codex exec` sandboxed read-only, plus the bus; which agents are installed and who conducts live in `src/agents.rs`, the pick in `~/.config/lama/config.json`) that only dispatches, and launches one owner agent per project it names, plus any peer an owner asks for — only in projects the lamaverse already shows — in kitty tabs, where each agent still asks before it edits or runs anything, giving each writer its own git worktree and branch under `<project>/.lama/worktrees/`; the `lama bus` MCP server they run writes only inside the task's directory (`~/.local/share/lama/tasks/<id>/`), and lama types bus messages into idle agents' windows. `src/registry.rs` keeps lama's own record of the agent sessions it sees running (process, kitty window, command line, first prompt, how each closed), one file per session under `~/.local/share/lama/sessions/`, so the sessions drawer (`src/sessions.rs`) can resume them. `src/config.rs` writes only `~/.config/lama/config.json`, when you pick a conductor or switch the theme (`p`). Nothing else writes anywhere.

Transcript formats enter through one boundary: `src/provider/` turns records into the facts in `src/fact.rs`, and nothing past it knows the format. See `docs/ARCHITECTURE.md` §0 before adding to either side.

See `docs/ARCHITECTURE.md` for the invariants and principles, `docs/DESIGN.md` for the module map and transcript format, `TODO.md` for the roadmap, and `README.md` for usage.

## Running

Two crates: `lama` at the root (the published lib + the `lama` binary) and `lama-web` in `web/wasm/` (the browser frontend, `publish = false`, wasm32-only). The browser frontend is **excluded** from the root workspace and resolves as its own — it cannot be compiled for the host at all, so keeping it a member would mean permanent phantom errors in the editor. The commands below therefore only touch `lama`; the browser frontend needs its own manifest path.

```bash
cargo build                     # native (default features)
cargo clippy                    # lint — must pass with no warnings
cargo fmt                       # format
cargo test                      # test

# the browser frontend (trunk → web/public/wasm/), lint it explicitly:
bash web/scripts/build-wasm.sh
cd web/wasm && cargo clippy   # its .cargo/config.toml defaults to wasm32
```

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), lowercase imperative subject. Scope is a module, not a file: `fact`, `provider`, `state`, `graph`, `timeline`, `tailer`, `ui`, `panel`, `cli`, `wasm`, `web`, `api`, `docs`. `api` is a change to what the crate exposes rather than to one module. A change inside one provider is `provider` (e.g. `fix(provider): inherit the timestamp across progress records`).

```
feat(timeline): index the playhead by event instead of wall-clock
fix(tailer): fold appends at the live edge without rebuilding
```

`cliff.toml` sets `filter_unconventional = true` — a non-conforming commit is dropped from the changelog. Lint with `committed HEAD~1..HEAD`, generate with `git cliff -o CHANGELOG.md`.
