---
title: Install
description: Install lama, the Claude Code and Codex session visualizer, with Homebrew, cargo or a prebuilt binary (or build it from source), or skip the install and run it in your browser.
---

lama runs two ways: a terminal app you install, or the same portable core
compiled to WebAssembly and running in your browser. The browser app needs no install at all.

## Run it in your browser

The fastest way to try lama is the browser app: it loads a demo session on
open, and from there you can browse your own sessions or drop in a transcript.

- **[Open the browser app →](/app)**

It runs entirely on your machine (it's WebAssembly served as a static page); your
transcripts are never uploaded. See [Usage & keys](/guides/usage/) for the controls,
which are identical to the native app.

## Install the terminal app

However you install it, the project is named `lama` and the command it gives
you is `lama`.

### Cargo

With a Rust toolchain (1.88 or newer):

```sh
cargo install --git https://github.com/AmalRNambiar/lama --locked
```

### From source

```sh
git clone https://github.com/AmalRNambiar/lama lama
cd lama
cargo build --release
# binary at ./target/release/lama
```

### Requirements

- The Homebrew and prebuilt routes need no toolchain at all.
- Building (`cargo install` or from source) needs a recent stable Rust toolchain
  (`rustup` recommended).
- A terminal that supports truecolor and mouse events (most modern terminals do).

## Build the browser app yourself

The browser frontend isn't part of the published crate — it's a separate,
unpublished crate, `lama-web`, that only builds for wasm32, so it's excluded
from the root workspace and no root `cargo` command touches it. It lives with this site under
[`web/`](https://github.com/AmalRNambiar/lama/tree/main/web) (the crate itself in
`web/wasm/`), and is compiled to wasm by [`trunk`](https://trunkrs.dev) and served
through Astro:

```sh
cd web
pnpm install
pnpm build          # builds the wasm, then the static site → web/dist/
pnpm dev            # or: run the dev server at http://localhost:4321
```

You'll need the `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`)
and `trunk` (`cargo install trunk`). `pnpm build:wasm` (that is,
`bash scripts/build-wasm.sh`) builds only the wasm; lint it from the repo root
with `cd web/wasm && cargo clippy` — that crate's `.cargo/config.toml` defaults the
target to wasm32, so no flags are needed.

## Status

Early and pre-release. It's usable for dogfooding your own sessions, but the keys,
CLI, and the on-disk format it reads may still shift. If something looks wrong,
please [open an issue](https://github.com/AmalRNambiar/lama/issues).
