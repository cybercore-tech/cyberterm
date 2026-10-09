<p align="center"><img src="assets/brand/cyberterm-hero.svg" alt="cyberterm" width="100%"></p>

# 📟 cyberterm

[![CI](https://github.com/cybercore-tech/cyberterm/actions/workflows/ci.yml/badge.svg)](https://github.com/cybercore-tech/cyberterm/actions/workflows/ci.yml)
[![Release](https://github.com/cybercore-tech/cyberterm/actions/workflows/release.yml/badge.svg)](https://github.com/cybercore-tech/cyberterm/actions/workflows/release.yml)

`Rust` · `wgpu` · `alacritty_terminal` · `glyphon`

**A GPU-rendered terminal emulator.** Real PTY handling and VT parsing via
`alacritty_terminal`, glyph rendering via `glyphon` (cosmic-text + etagere
on a wgpu pipeline), the Kitty keyboard protocol, shell integration with
prompt jumping, 12 built-in Kitty-format themes plus the shared Cybercore
theme catalog, and a config file that applies the moment you save it. No
Electron, no bundled shell.

**[cybercore-tech.github.io/cyberterm](https://cybercore-tech.github.io/cyberterm/)**

## 📦 Install

```bash
curl -fsSL https://raw.githubusercontent.com/cybercore-tech/cyberterm/main/install.sh | sh
```

Downloads the latest release for your platform (Linux x86_64/aarch64,
or macOS aarch64/Apple Silicon), verifies its SHA-256 checksum, and
installs `cyberterm` to `~/.local/bin`. Linux needs the usual desktop
GL/X11/Wayland libraries already present — nothing extra to install for
the binary itself. No Intel Mac build — GitHub's `macos-13` runner
queue capacity has been too degraded to build one reliably in CI; build
from source with `cargo build --release` there instead.

## 🚀 Commands

```bash
cyberterm                          # launch the terminal
cyberterm +list-themes             # list every theme found in ~/.config/cyberterm/themes
cyberterm +set-theme <name>        # switch theme and save it as the default
cyberterm +set-opacity --custom=0.85
cyberterm +edit-theme --create-theme <category> <folder> <name>
cyberterm +edit-theme --view-theme <category> <folder> <name>
cyberterm +edit-theme --remove-theme <category> <folder> <name>
cyberterm +list-termkeys           # the effective keybindings, including your overrides
cyberterm +default-config          # a commented config with every default
cyberterm +shell-integration zsh   # the integration script for zsh, bash or fish
```

## ⌨️ Keys and mouse

| Keys | Action |
|---|---|
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste (bracketed paste when the program asks for it) |
| `Ctrl+Insert` / `Shift+Insert` | Copy / paste, so Omarchy's universal Super+C / Super+V work |
| Middle click | Paste the primary selection |
| `Ctrl+Shift+A` | Select the whole scrollback |
| `Shift+PageUp` / `Shift+PageDown` | Scroll a page |
| `Ctrl+Shift+Up` / `Ctrl+Shift+Down` | Scroll a line |
| `Shift+Home` / `Shift+End` | Top of the scrollback / back to the live screen |
| `Ctrl+Shift+Z` / `Ctrl+Shift+X` | Jump to the previous / next shell prompt |
| `Ctrl+Shift+K` | Clear the scrollback |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Bigger / smaller / reset font |
| `Ctrl+Shift+T` | Theme picker |
| `Ctrl+Shift+R` | Reload the config now |

Rebind or free any of them under `[keybindings]` (`"ctrl+shift+k" = "none"`
hands that combo back to the shell).

Mouse: drag to select (double-click for a word, triple-click for a line,
Alt+drag for a block, Shift+click to extend). Finished selections are copied
to the primary selection by default; set `[mouse] copy_on_select` to
`"clipboard"` or `"both"` to have them land on the regular clipboard too.
Right-click opens a menu with Copy, Paste, Select All, Open/Copy Link,
Clear Scrollback, Themes and Reload Config (Shift+right-click while a
program has mouse reporting on). The wheel scrolls the scrollback; in full-screen programs
without mouse support (`less`, `man`) it sends arrow keys. Programs that
turn on mouse reporting (vim, htop, tmux, fzf) get clicks, drags and the
wheel in SGR, UTF-8 or X10 encoding; hold Shift to select text anyway.
Hyperlinks, both OSC 8 links and plain `https://` URLs, underline on hover
and open with Ctrl+click.

## 🐚 Shell integration

```bash
eval "$(cyberterm +shell-integration zsh)"    # ~/.zshrc
eval "$(cyberterm +shell-integration bash)"   # ~/.bashrc
cyberterm +shell-integration fish | source    # ~/.config/fish/config.fish
```

The script makes the shell report its working directory (OSC 7) and mark
each prompt and command (OSC 133). That's what powers prompt jumping now, and
what command blocks, "open a split here" and exit-code tracking will build on.
The marks are stored on the terminal cells themselves, so they scroll,
reflow on resize and age out of the scrollback with the text they belong to.
The escapes are harmless in other terminals.

## 🛠 Configuration

`~/.config/cyberterm/cyber_config.toml`. Every key is optional, and changes
apply within a second of saving. Run `cyberterm +default-config` for the full
commented reference:

```toml
theme = "synthwave_84"
opacity = 0.9

[font]
family = "JetBrainsMono Nerd Font"
size = 9.0            # points, like Ghostty/Kitty/Alacritty
line_height = 1.25
fallback = ["Symbols Nerd Font Mono", "Noto Color Emoji"]

[cursor]
style = "block"       # block | beam | underline
blinking = false

[scrollback]
lines = 10000

[clipboard]
osc52 = "copy"        # disabled | copy | paste | copy-paste

[keybindings]
"ctrl+alt+c" = "copy"
```

12 curated Kitty-syntax themes ship built-in (`~/.config/cyberterm/themes/*.conf`)
and load unmodified, so anything pulled straight from
[kovidgoyal/kitty-themes](https://github.com/kovidgoyal/kitty-themes) drops in
next to them with zero conversion. `+edit-theme` writes your own custom
themes as JSON under a `category/folder/` layout alongside the built-ins.
Press `Ctrl+Shift+T` in the terminal to open the theme picker.

Cyberterm also loads the shared Cybercore theme catalog. Its semantic
background, foreground, and accent colors map to the terminal's 16 ANSI
slots; Cyberterm's local Kitty and JSON themes remain available in the same
picker. Choosing a shared theme saves the selection to the shared Cybercore
catalog, so compatible Cybercore apps follow that selection too. Cyberterm
uses the published `cybercore` 0.8 crate and checks the shared catalog
revision once a second, so updates from Theme Studio or another Cybercore
app apply without a restart.

## ⚙️ Rendering

- **Text:** `glyphon` (cosmic-text shaping + an etagere glyph atlas) on wgpu,
  with bold, italic, dim, strikethrough and five underline styles (single,
  double, curly, dotted, dashed), including colored underlines.
- **Alignment:** each row is cut into ASCII runs plus individually placed
  wide and non-ASCII glyphs (CJK, emoji, Nerd Font icons, combining marks).
  A fallback font with a different advance width can't push the rest of the
  line out of alignment.
- **Shaping cache:** shaping is cached by row content, so scrolling and
  redraws don't re-shape unchanged text.
- **Fonts:** only the configured fonts are loaded at startup. The first time
  a character none of them covers appears, fontconfig finds a font that has
  it.
- **Box drawing:** box-drawing and block-element characters are drawn as
  rectangles from the cell geometry, so TUI borders join seamlessly at any
  font size or line height.
- **Opacity:** window opacity fades cell backgrounds only. Text stays fully
  opaque, matching how Ghostty/Kitty/Alacritty do transparency.

## 🗺 Known limitations

- One pane per window for now: no tabs or splits yet. The code is
  structured for them (panes are independent sessions), and they're next on
  the roadmap.
- No scrollback search yet.
- No sixel or Kitty graphics protocol (inline images) yet.

## 🚦 Quality gate

```bash
./scripts/release-gates quick   # fmt + check + clippy
./scripts/release-gates full    # quick + tests + strict rustdoc
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for what each step checks and why
the render/PTY/input path can't be covered by automated tests.

## ⚖️ Namespace & Legal Attribution

This project is an independent component of the **Cybercore Systems Framework** hosted canonically at [cybercore-tech.github.io](https://cybercore-tech.github.io/).

**Copyright (c) 2026 Cybercore Tech (cybercore-tech.github.io)**

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS-IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

### 🛡️ Defensive Guardrail Statement

All software components, tools, prefixes, and configurations under the "Cyber" prefix within this ecosystem are developed completely independently as open-source utilities for specialized terminal environments. They maintain absolutely no affiliation, partnership, endorsement, sponsorship, or commercial connection with any external corporate cybersecurity providers, training collectives, or federal defense contractors. Prior art is formally registered and maintained immutably via active domain publication.

**Contact:** [cybercore.sh+cyberterm@gmail.com](mailto:cybercore.sh+cyberterm@gmail.com) // [cybercore-tech.github.io](https://cybercore-tech.github.io/)
