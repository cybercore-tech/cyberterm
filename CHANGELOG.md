# Changelog

## Unreleased

### Added
- **Agent sessions** (`cyberterm +agent <name> [task]`, or **New agent** in the command palette): run any coding agent in its own git worktree and tab.
  - The agents on your PATH are found automatically (Claude Code, Codex, Copilot, Gemini CLI, Cursor Agent, opencode, Crush and more), and anything else can be added under `[agents.launch]`.
  - Agent tabs are titled and badged, including in daemon sessions.
  - `+agent list` shows each session's state; `+agent rm` cleans up without losing work.
- **Flight log** (`cyberterm +agent log [id] [-o] [-f]`): what an agent session did, including prompts, commands with exit codes, durations and output, edits, and waiting/done.
  - **Any agent:** a shell recorder covers any agent that uses bash or zsh.
  - **Native hooks:** Claude Code gets hooks for its session only.
  - **Open format:** `cyberterm +hook event` takes events from anything else.
- **Flight log panel** (`Ctrl+Shift+L`): the log beside the agent's tab, opening by itself for new agents.
  - **Live state:** working, needs you, done or stopped, with counts and the timeline; failed commands show their last output line, and Enter opens a command's output.
  - **Tab marks:** `◆` working, `◆!` needs you, `✓` done.
  - **Notifications:** a desktop notification when an agent you're not looking at needs you or has finished.
- **Desktop notifications from programs** (OSC 9 and OSC 777) are understood. An agent sending one shows as needing you.

## 0.2.1 — 2026-10-10

### Added
- **Command palette** (`Ctrl+Shift+P`): fuzzy search over every action, Lua commands, open panes and tabs, themes and recent commands.
- **129 themes out of the box:** 36 popular themes (Catppuccin, Tokyo Night, Gruvbox, Rosé Pine, Dracula, Nord, Solarized, One Dark/Light, Kanagawa, Everforest, …) join the 12 built-in and 81 Cybercore ones.
- **`cyberterm +themes`:** install optional collections, 1,500+ themes in total (iTerm2-Color-Schemes 768, kitty-themes 418, Cyberterm extras 203).
- **Rebuilt theme picker:** type to filter, themes grouped by family, live preview with sample output, Esc restores your previous theme.
- **Cybercore themes get real bright colors.**

### Fixed
- Text was drawn darker than its color: mid-tones lost contrast in every theme.
- Inline images wider than the window didn't show with `kitten icat`; its fast file-transfer mode was also refused.
- Creating a theme from the picker dropped the Cybercore themes from the list.

## 0.2.0 — 2026-10-10

First release with the full feature set: splits, tabs and sessions; command blocks and searchable history; AI in both directions (MCP bridge, explain and ask); danger mode; SSH-aware splits; live ports; rewind; inline images; Lua scripting.
