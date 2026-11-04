# Changelog

## 0.2.2 - 2026-11-04

### Changed

- **Task agents use the lama bus without asking**: Claude workers start with
  the bus's tools allowed, and Codex workers with them approved, so a new task
  no longer asks you to allow `send`, `report` and the rest. Everything else
  still asks.

### Fixed

- A command you allowed the conductor in a task isn't asked about again when
  it retries it, and an allowed command that fails comes back to you as one
  line with the error instead of another prompt.
- A permission prompt left open by a conductor turn that ended is closed
  instead of shown.

## 0.2.1 - 2026-11-04

### Added

- **Ask who runs a task up front**: with both Claude Code and Codex installed,
  the intake asks who runs the task's agents (Claude, Codex, or the
  conductor's pick) as you hand it over, so the team launches without waiting
  on you.
- **The conductor can ask for tools**: a tool outside its read-only set no
  longer gets refused silently. The intake shows what it wants to run; `y`
  allows it, `n` refuses, or type why not.

### Fixed

- CI passes on Linux, macOS and Windows, on the MSRV, and for the wasm build.

## 0.2.0 - 2026-11-03

The first public release of lama, a fork of
[zoetrope](https://github.com/furkankly/zoetrope) 0.2.0 by Furkan Kalaycioglu.

### Added

- **Lamaverse**: every project's Claude Code and Codex sessions on one canvas,
  with a project strip, free card arrangement kept across restarts, and a
  timeline lane per project.
- **Session console**: a session's conversation on the canvas, a prompt line
  that types into the agent's own window, approval questions answered by key
  or click, stop and resume.
- **New sessions** from the canvas and a **sessions drawer** that resumes
  closed ones.
- **Tasks**: a conductor dispatches a ticket to owner agents per project, each
  writer in its own git worktree, talking over a message bus.
- **kitty and tmux** bridges for reaching agents' terminals.
- Works with Claude Code, Codex, or both; Claude or Codex can conduct tasks.
- The **lama** mascot, the **standard** theme beside the cyberpunk one, and an
  opt-in "night edition" news feed.
