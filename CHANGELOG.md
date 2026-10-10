# Changelog

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
