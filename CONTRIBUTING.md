# 🤝 Contributing to Cyberterm

`Rust` · `wgpu` · `alacritty_terminal` · `glyphon` · `winit`

Cyberterm is a single bin crate, not a workspace, so this gate is a scaled-
down version of the multi-crate one used elsewhere in this org (see
[diagprint](https://github.com/cybercore-tech/diagprint) for the full
workspace/MSRV-matrix version): no per-package arrays, no publish step (a
terminal emulator binary isn't published to crates.io), and no pinned-MSRV
re-run (that matters for a *library* other people compile with their own
toolchain constraints — a bin only needs to build with whatever toolchain
built it).

## 🚦 Quality gate

Run before every commit that touches `src/`:

```bash
./scripts/release-gates quick   # fmt + check + clippy -- fast, run constantly
./scripts/release-gates full    # quick + tests + strict rustdoc
```

<details>
<summary>What each step does and what a failure usually means</summary>

| Step | Command | What it means if it fails |
|---|---|---|
| Whitespace | `git diff --check` | Trailing whitespace or conflict markers snuck into a diff |
| Format | `cargo fmt --all -- --check` | Code doesn't match the project's `rustfmt` style — run `cargo fmt --all` to fix |
| Check | `cargo check --all-targets --all-features` | Doesn't compile (including tests) |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | A real lint issue — this repo treats every clippy warning as an error, no exceptions |
| Tests | `cargo test --bin cyberterm --all-features` | A unit test regressed — mostly the pure, headlessly-testable logic (theme file parsing, key-to-PTY-byte encoding) since the GUI/GPU/shell path itself needs a real window and can't run in CI |
| Docs | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --bin cyberterm` | A broken doc link or invalid rustdoc syntax |

</details>

CI (`.github/workflows/ci.yml`) mirrors the same checks in separate jobs
(`quality`, `test`) rather than shelling out to the script — the script is
what you run locally before pushing so CI isn't the first signal.

## 🖥️ Why some things can't be tested in CI

Cyberterm opens a real GPU window and spawns a real shell over a PTY. Neither
of those can be driven headlessly in CI or a sandboxed agent shell — there's
no display, no GPU adapter, and no interactive keyboard focus. What *is*
tested automatically:

- [x] Theme file parsing (`src/theme.rs`) and the config schema, defaults
      and comment-preserving saves (`src/config.rs`)
- [x] Key encoding, legacy xterm and the Kitty keyboard protocol
      (`src/input/keyboard.rs`); mouse reports in SGR/UTF-8/X10
      (`src/input/mouse.rs`); bracketed paste (`src/input/paste.rs`);
      keybinding parsing and overrides (`src/input/bindings.rs`); URL
      detection (`src/input/links.rs`)
- [x] Terminal state to render cells (`src/frame.rs`): real escape
      sequences are fed into a real `alacritty_terminal::Term` and the
      resulting cells checked (SGR attributes, 256/truecolor, OSC color
      overrides, wide chars, combining marks, cursor shapes, selection,
      scrollback viewport, OSC 8 links)
- [x] Shell integration: the OSC 7/133 stream tap (`src/shell/tap.rs`),
      prompt marks surviving scroll into history, and the zsh/bash/fish
      scripts parsing in their own shells (skipped if a shell isn't installed)
- [x] Box-drawing/block geometry (`src/boxdraw.rs`)
- [x] Split-tree layout: split, close, resize, equalize, directional
      focus, dividers, zoom (`src/layout.rs`); layout files
      (`src/layout_file.rs`); tab bar (`src/ui/tab_bar.rs`); context menu
      (`src/ui/context_menu.rs`); control-socket protocol (`src/control.rs`)
- [x] Session daemon: snapshot/replay round trips (scrollback, attributes,
      links, prompt marks, modes, palette, the alternate screen;
      `src/mux/snapshot.rs`), the wire protocol (`src/mux/protocol.rs`), and
      an end-to-end test that runs a real daemon and `/bin/sh` on a
      temporary socket, detaches, reattaches and checks the shell survived
      (`src/mux/server.rs`)
- [ ] Actually rendering a frame, spawning a shell, or receiving keystrokes:
      these need a human to launch the binary and look at the window

When you touch the render/PTY/input path, say so in your PR description and
note what manual verification you did (launched it, typed a command, ran
`ls --color`/`vim`/`htop`, resized the window, etc.) — that's the closest
thing to a test for that code today.

## 📄 License

See [`README.md`](README.md).
