# Changelog

## Unreleased

### Added
- **Several agents on one task** (`cyberterm +agent claude,codex,gemini "task"`): one attempt each, in their own worktrees and tabs, grouped in the Tower so you can compare them. Merging one offers to discard the rest.
- **Quit and merge:** for an agent that finished but is still open, the Tower's merge and discard offer to quit it first, then close its tab.
- **The Tower** (`Ctrl+Shift+S`): every agent session at once.
  - **What it shows:** the sessions on the left (state, agent, branch, lines changed); the selected one's flight log on the right.
  - **Keys:** **↵** jumps to its tab, **c** opens its Changes, **m** merges it, **d** discards it (both after asking), **n** starts a new agent.
- **Merge or discard an agent's work** (`cyberterm +agent merge <id>`, `cyberterm +agent discard <id>`).
  - **Merge** brings the session's branch into the branch it started from: one squashed commit by default, or a merge commit (`--merge`), or a fast-forward (`--ff`).
  - **Safe:** it checks everything first and changes nothing if it can't finish (agent still running, your checkout busy or on another branch, conflicts found with `git merge-tree`). `--check` previews.
  - **Discard** removes the worktree, branch and session after asking.
- **Copilot CLI, Cursor and Hermes report to the flight log** like Claude Code, Codex and Gemini CLI do: prompts, command output and exit codes, edits, other tools, and when they need you or are done. Each appears lit in the agent home once it reports.
  - **Copilot CLI:** hooks per session, through a plugin of Cyberterm's own; nothing to set up.
  - **Cursor:** `cyberterm +agent setup cursor` adds them to `~/.cursor/hooks.json`.
  - **Hermes:** `cyberterm +agent setup hermes` adds a plugin and enables it through Hermes's own `hermes plugins enable`.
  - As with Gemini, what setup installs does nothing outside Cyberterm's agent sessions, and `--remove` takes it out.
- **The agent home** (`cyberterm +agent` on its own): the Cyberterm wordmark in your theme's colours, the agents found here with how much each tells the flight log (and the setup step that would give more), your agent sessions with their state and lines changed, and the keys for the Flight log and Changes. Piped, `+agent` still prints the plain listing.
- **Codex and Gemini CLI report to the flight log** like Claude Code does: prompts, command output and exit codes, edits, other tools, and when they need you or are done.
  - **Codex:** hooks per session; you approve them once in Codex.
  - **Gemini:** `cyberterm +agent setup gemini` installs them; they're inert outside Cyberterm.
- **`cyberterm +agent setup`** shows what each agent reports and installs the opt-in hooks; `--remove` takes them out.

### Fixed
- Messages from the Changes view (like "Reverted …") were hidden behind it.
- `+agent rm` left the session's flight log behind.
- The shell recorder logged agents running Cyberterm's own hooks as commands.

## 0.3.0 — 2026-10-10

**The agent workspace:** run any coding agent in its own worktree, see what it does as it works, and review what it changed.

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
- **Changes view** (`Ctrl+Shift+M`, or `c` in the Flight log): what the agent in this tab changed since it started.
  - **What it shows:** commits, uncommitted work and new files, as files with `+`/`−` counts and a diff with line numbers; it refreshes while the agent works.
  - **Keys:** **e** opens a file at its first change; **r** reverts it, after asking.
- **Desktop notifications from programs** (OSC 9 and OSC 777) are understood. An agent sending one shows as needing you.

### Fixed
- An unknown `+command` (for example, run with an older cyberterm on PATH) opened a window as if it worked; it now says so and exits.

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
