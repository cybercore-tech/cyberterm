// src/app/blocks.rs
//
// Command blocks in the window: the gutter bar and exit/duration badge on
// each block, the block actions (copy, rerun, watch, diff, show output),
// saving finished commands to history, and notifying when a long command
// finishes out of sight.

use super::*;
use crate::blocks::{self as blk, BlockSpan};
use crate::history;
use crate::layout::Direction;
use crate::shell::BlockMeta;

/// Where a block action came from: a pane and the block's mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub pane: PaneId,
    pub mark: u64,
}

pub(super) fn shell_name(program: Option<&str>) -> String {
    program
        .map(str::to_string)
        .or_else(|| std::env::var("SHELL").ok())
        .and_then(|p| p.rsplit('/').next().map(str::to_string))
        .unwrap_or_else(|| "sh".into())
}

impl App {
    // ------------------------------------------------------------------
    // Drawing
    // ------------------------------------------------------------------

    /// Adds a pane's block gutter bars and badges to its frame.
    pub(super) fn decorate_blocks(
        &self,
        pane: &Pane,
        frame: &mut Frame,
        rect: Rect,
        overlays: &mut Vec<Overlay>,
    ) {
        if !self.config.blocks.decorations {
            return;
        }
        let Some(gpu) = &self.gpu else { return };
        let (_, ch) = gpu.renderer.cell_size();
        let term = pane.session.term.lock();
        let shell = pane.session.shell.lock();
        if shell.prompts == 0 {
            return;
        }
        let offset = term.grid().display_offset() as i32;
        let rows = frame.rows as i32;
        let (top, bottom) = (-offset, rows - 1 - offset);
        let scale = gpu.window.scale_factor() as f32;
        let bar_w = (2.0 * scale).round().max(1.0);
        let bar_x = rect.x - (self.padding(gpu).min(3.0 * scale)).max(bar_w);

        for span in blk::spans_in(&*term, top, bottom) {
            let Some(meta) = blk::meta(&shell, span.mark) else {
                continue;
            };
            let color = self.block_color(meta);
            let first = span.prompt.max(top);
            let last = (span.end - 1).min(bottom);
            if first <= last {
                overlays.push(Overlay {
                    rect: Rect {
                        x: bar_x,
                        y: rect.y + (first + offset) as f32 * ch,
                        w: bar_w,
                        h: (last - first + 1) as f32 * ch,
                    },
                    color,
                    alpha: 0.9,
                });
            }
            let badge_row = span.command_end + offset;
            if (0..rows).contains(&badge_row) {
                self.draw_badge(frame, badge_row as usize, meta, color);
            }
        }
    }

    fn block_color(&self, meta: &BlockMeta) -> [u8; 3] {
        let ansi = |i: usize| frame::hex_to_rgb(self.palette.ansi[i]);
        match (meta.running(), meta.exit) {
            (true, _) => ansi(3),
            (false, Some(0)) => ansi(2),
            (false, Some(_)) => ansi(1),
            (false, None) => ansi(8),
        }
    }

    /// `✓ 1.2s` / `✗ 2 · 3.4s` / `… 12s`, right-aligned on the command row
    /// when those cells are empty.
    fn draw_badge(&self, frame: &mut Frame, row: usize, meta: &BlockMeta, color: [u8; 3]) {
        let duration = meta
            .duration_ms()
            .map(blk::format_duration)
            .unwrap_or_default();
        let text = match (meta.running(), meta.exit) {
            (true, _) => format!(" … {duration} "),
            (false, Some(0)) => format!(" ✓ {duration} "),
            (false, Some(code)) => format!(" ✗ {code} · {duration} "),
            (false, None) => return,
        };
        let len = text.chars().count();
        let cols = frame.cols;
        if cols < len + 4 {
            return;
        }
        let start = cols - len - 1;
        let cells = &mut frame.cells[row * cols..(row + 1) * cols];
        if cells[start..]
            .iter()
            .any(|c| c.ch != ' ' || c.bg != frame.bg)
        {
            return;
        }
        for (i, ch) in text.chars().enumerate() {
            let cell = &mut cells[start + i];
            cell.ch = ch;
            cell.fg = color;
        }
    }

    // ------------------------------------------------------------------
    // Lookup
    // ------------------------------------------------------------------

    /// The command block under a viewport row of the focused pane.
    pub(super) fn block_under(&self, row: usize) -> Option<(BlockRef, BlockMeta)> {
        let pane = self.focused_pane()?;
        let term = pane.session.term.lock();
        let line = row as i32 - term.grid().display_offset() as i32;
        let span = blk::block_at(&*term, line)?;
        let shell = pane.session.shell.lock();
        let meta = blk::meta(&shell, span.mark)?.clone();
        Some((
            BlockRef {
                pane: pane.id,
                mark: span.mark,
            },
            meta,
        ))
    }

    /// The newest finished command in the focused pane.
    fn last_finished(&self) -> Option<(BlockRef, BlockMeta)> {
        let pane = self.focused_pane()?;
        let shell = pane.session.shell.lock();
        let meta = shell
            .blocks
            .iter()
            .rev()
            .find(|b| b.is_command() && b.finished_ms.is_some())?
            .clone();
        Some((
            BlockRef {
                pane: pane.id,
                mark: meta.mark,
            },
            meta,
        ))
    }

    pub(super) fn span_and_output(&self, block: BlockRef) -> Option<(BlockSpan, String)> {
        let pane = self.pane(block.pane)?;
        let term = pane.session.term.lock();
        let span = blk::span_of(&*term, block.mark)?;
        let text = blk::output_text(&*term, &span);
        Some((span, text))
    }

    /// The output of the previous run of the same command: an earlier block
    /// in the same pane, else the history database.
    pub(super) fn previous_output(&self, block: BlockRef, meta: &BlockMeta) -> Option<String> {
        let command = meta.command.as_deref()?;
        if let Some(pane) = self.pane(block.pane) {
            let earlier: Option<u64> = pane
                .session
                .shell
                .lock()
                .blocks
                .iter()
                .rev()
                .filter(|b| b.mark < block.mark && b.command.as_deref() == Some(command))
                .map(|b| b.mark)
                .next();
            if let Some(mark) = earlier {
                let term = pane.session.term.lock();
                if let Some(span) = blk::span_of(&*term, mark) {
                    return Some(blk::output_text(&*term, &span));
                }
            }
        }
        if !self.config.history.enabled {
            return None;
        }
        let store = history::Store::open(&history::default_path()).ok()?;
        let cwd = meta.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
        store
            .previous_output(command, cwd.as_deref(), meta.started_ms.unwrap_or(u64::MAX))
            .ok()
            .flatten()
    }

    // ------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------

    pub(super) fn copy_block_output(&mut self, block: BlockRef) {
        if let Some((_, text)) = self.span_and_output(block) {
            if let Some(clipboard) = &mut self.clipboard {
                clipboard.set(
                    ClipKind::Clipboard,
                    &text,
                    self.config.clipboard.clean_box_drawing,
                );
            }
        }
    }

    pub(super) fn copy_last_output(&mut self) {
        if let Some((block, _)) = self.last_finished() {
            self.copy_block_output(block);
        }
    }

    pub(super) fn copy_text(&mut self, text: &str) {
        if let Some(clipboard) = &mut self.clipboard {
            clipboard.set(ClipKind::Clipboard, text, false);
        }
    }

    /// Runs a command again in its pane, or in a new pane if that one is
    /// busy.
    pub(super) fn rerun(&mut self, block: BlockRef, command: &str) {
        let busy = self
            .pane(block.pane)
            .is_none_or(|p| p.session.shell.lock().command_running);
        if busy {
            self.run_in_split(Direction::Right, command, None);
            return;
        }
        if let Some(pane) = self.pane(block.pane) {
            pane.session.term.lock().scroll_display(Scroll::Bottom);
            pane.session.write(format!("{command}\r").into_bytes());
        }
        self.focus_pane(block.pane);
    }

    /// Opens a split in `cwd` (default: the focused shell's) running
    /// `command`.
    pub(super) fn run_in_split(&mut self, dir: Direction, command: &str, cwd: Option<PathBuf>) {
        let cwd = cwd
            .filter(|c| c.is_dir())
            .or_else(|| self.focused_pane().and_then(|p| p.session.cwd()));
        match self.split_with(dir, cwd) {
            Ok(id) => self.run_in(id, Some(command)),
            Err(e) => eprintln!("cyberterm: couldn't open a pane: {e}"),
        }
    }

    /// A pane that reruns the command every two seconds.
    pub(super) fn watch(&mut self, command: &str, cwd: Option<PathBuf>) {
        let looped = if shell_name(self.config.shell.program.as_deref()) == "fish" {
            format!("while true; clear; {command}; sleep 2; end")
        } else {
            format!("while true; do clear; {command}; sleep 2; done")
        };
        self.run_in_split(Direction::Down, &looped, cwd);
    }

    /// Opens a pane showing `diff -u` between the previous run's output and
    /// this one's.
    pub(super) fn diff_with_previous(&mut self, block: BlockRef, meta: &BlockMeta) {
        let (Some(previous), Some((_, current))) = (
            self.previous_output(block, meta),
            self.span_and_output(block),
        ) else {
            return;
        };
        let Some((old, new)) = write_temp_pair(block, &previous, &current) else {
            return;
        };
        let label = meta.command.clone().unwrap_or_default().replace('\'', "");
        // The leading space keeps viewer commands out of history (ours and
        // the shell's ignorespace convention).
        let command = format!(
            " diff -u --color=always --label 'previous: {label}' --label 'this run' '{}' '{}' | less -R",
            old.display(),
            new.display()
        );
        self.run_in_split(Direction::Down, &command, meta.cwd.clone());
    }

    pub(super) fn block_output_is_json(&self, block: BlockRef) -> bool {
        self.span_and_output(block)
            .is_some_and(|(_, text)| crate::json_viewer::looks_like_json(&text))
    }

    /// Opens a block's JSON output in `cyberterm +json`, in a split.
    pub(super) fn view_json(&mut self, block: BlockRef) {
        let Some((_, text)) = self.span_and_output(block) else {
            return;
        };
        let (Some((_, file)), Ok(exe)) =
            (write_temp_pair(block, "", &text), std::env::current_exe())
        else {
            return;
        };
        let command = format!(
            " {} +json {}",
            shell_quote(&exe.to_string_lossy()),
            shell_quote(&file.to_string_lossy())
        );
        self.run_in_split(Direction::Down, &command, None);
    }

    /// The newest finished command's output in a pager, in a split.
    pub(super) fn show_last_output(&mut self) {
        let Some((block, _)) = self.last_finished() else {
            return;
        };
        let Some((_, text)) = self.span_and_output(block) else {
            return;
        };
        self.show_text_in_pager_for(block, &text);
    }

    /// Saved output (e.g. from history) in a pager pane.
    pub(super) fn show_text_in_pager(&mut self, text: &str, id: i64) {
        let block = BlockRef {
            pane: u32::MAX,
            mark: id as u64,
        };
        self.show_text_in_pager_for(block, text);
    }

    fn show_text_in_pager_for(&mut self, block: BlockRef, text: &str) {
        let Some((_, file)) = write_temp_pair(block, "", text) else {
            return;
        };
        self.run_in_split(
            Direction::Down,
            &format!(" less -R '{}'", file.display()),
            None,
        );
    }

    // ------------------------------------------------------------------
    // History and notifications
    // ------------------------------------------------------------------

    /// After new output in a pane: save finished commands (for panes this
    /// window owns; the daemon saves its own) and notify about long ones.
    pub(super) fn blocks_changed(&mut self, index: usize) {
        let pane = &self.panes[index];
        if let (Some(recorder), false) = (&self.history, pane.session.is_remote()) {
            let records = {
                let term = pane.session.term.lock();
                let mut shell = pane.session.shell.lock();
                history::collect(&*term, &mut shell, "local", &self.history_policy)
            };
            recorder.record(records);
        }

        // Lua: commands that started since last time.
        let started: Vec<BlockMeta> = {
            let pane = &self.panes[index];
            let shell = pane.session.shell.lock();
            shell
                .blocks
                .iter()
                .filter(|b| b.mark > pane.started_mark && b.is_command() && b.started_ms.is_some())
                .cloned()
                .collect()
        };
        if let Some(newest) = started.iter().map(|b| b.mark).max() {
            let id = self.panes[index].id;
            self.panes[index].started_mark = newest;
            for b in &started {
                self.lua_emit("command_started", block_event(id, b));
            }
        }
        let pane = &self.panes[index];

        let threshold = self.config.notify.long_command_seconds;
        let finished: Vec<BlockMeta> = {
            let shell = pane.session.shell.lock();
            shell
                .blocks
                .iter()
                .filter(|b| {
                    b.mark > pane.notified_mark && b.is_command() && b.finished_ms.is_some()
                })
                .cloned()
                .collect()
        };
        let Some(newest) = finished.iter().map(|b| b.mark).max() else {
            return;
        };
        let (id, seen) = (pane.id, self.pane_in_view(pane.id));
        self.panes[index].notified_mark = newest;
        for b in &finished {
            self.lua_emit("command_finished", block_event(id, b));
        }
        if threshold == 0 || seen {
            return;
        }
        for b in finished {
            if b.duration_ms().unwrap_or(0) >= threshold * 1000 {
                notify_finished(&b, id);
            }
        }
    }

    /// Whether the user is looking at this pane right now.
    fn pane_in_view(&self, id: PaneId) -> bool {
        self.window_focused && id == self.focused
    }
}

/// `$XDG_RUNTIME_DIR/cyberterm/block-<pid>-<pane>-<mark>-{a,b}.txt`.
/// A command block as a Lua event.
fn block_event(pane: PaneId, b: &BlockMeta) -> serde_json::Value {
    serde_json::json!({
        "pane": pane,
        "command": b.command,
        "cwd": b.cwd,
        "exit": b.exit,
        "started_ms": b.started_ms,
        "finished_ms": b.finished_ms,
        "duration_ms": b.duration_ms(),
    })
}

fn write_temp_pair(block: BlockRef, a: &str, b: &str) -> Option<(PathBuf, PathBuf)> {
    let dir = crate::control::prepare_socket_dir().ok()?;
    let stem = format!("block-{}-{}-{}", std::process::id(), block.pane, block.mark);
    let (pa, pb) = (
        dir.join(format!("{stem}-a.txt")),
        dir.join(format!("{stem}-b.txt")),
    );
    std::fs::write(&pa, format!("{a}\n")).ok()?;
    std::fs::write(&pb, format!("{b}\n")).ok()?;
    Some((pa, pb))
}

fn notify_finished(b: &BlockMeta, _pane: PaneId) {
    let command = b.command.clone().unwrap_or_default();
    let short: String = command.chars().take(60).collect();
    let duration = b
        .duration_ms()
        .map(blk::format_duration)
        .unwrap_or_default();
    let (title, body) = match b.exit {
        Some(0) => (format!("✓ {short}"), format!("finished in {duration}")),
        Some(code) => (
            format!("✗ {short}"),
            format!("exit {code} after {duration}"),
        ),
        None => (short, format!("finished in {duration}")),
    };
    let body = match &b.cwd {
        Some(cwd) => format!("{body} · {}", cwd.display()),
        None => body,
    };
    spawn_detached(
        Command::new("notify-send")
            .arg("--app-name=Cyberterm")
            .arg(title)
            .arg(body),
    );
}

impl App {
    /// Opens a file reference in the user's editor at its line.
    pub(super) fn open_file(&mut self, file: &FileTarget) {
        let (argv, in_pane) = editor_command(
            self.config.links.editor.as_deref(),
            self.config.links.editor_in_pane,
            file,
        );
        if argv.is_empty() {
            return;
        }
        if in_pane {
            let line = argv
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            let dir = file.path.parent().map(PathBuf::from);
            self.run_in_split(Direction::Right, &format!(" {line}"), dir);
        } else {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            spawn_detached(&mut command);
        }
    }
}

/// Editors that open their own window rather than running in a terminal.
const GUI_EDITORS: &[&str] = &[
    "code",
    "codium",
    "code-insiders",
    "cursor",
    "windsurf",
    "zed",
    "zeditor",
    "subl",
    "gedit",
    "kate",
];

/// The command line to open `file` at its line, and whether it runs in a
/// pane. `template` (config) wins; otherwise $VISUAL / $EDITOR, using each
/// editor's own line-number syntax.
fn editor_command(
    template: Option<&str>,
    in_pane: Option<bool>,
    file: &FileTarget,
) -> (Vec<String>, bool) {
    let path = file.path.to_string_lossy().into_owned();
    let (line, col) = (file.line.to_string(), file.col.unwrap_or(1).to_string());
    if let Some(t) = template.filter(|t| !t.trim().is_empty()) {
        let argv: Vec<String> = t
            .split_whitespace()
            .map(|w| {
                w.replace("{file}", &path)
                    .replace("{line}", &line)
                    .replace("{col}", &col)
            })
            .collect();
        let gui = argv
            .first()
            .is_some_and(|e| GUI_EDITORS.contains(&base_name(e)));
        return (argv, in_pane.unwrap_or(!gui));
    }
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "nano".into());
    let mut words: Vec<String> = editor.split_whitespace().map(str::to_string).collect();
    let name = base_name(&words[0]).to_string();
    let at = format!("{path}:{line}:{col}");
    let (args, gui): (Vec<String>, bool) = match name.as_str() {
        "code" | "codium" | "code-insiders" | "cursor" | "windsurf" => {
            (vec!["-g".into(), at], true)
        }
        "zed" | "zeditor" | "subl" => (vec![at], true),
        "hx" | "helix" => (vec![at], false),
        "kak" => (vec![format!("+{line}:{col}"), path], false),
        "nano" => (vec![format!("+{line},{col}"), path], false),
        "micro" => (vec![at], false),
        _ => (vec![format!("+{line}"), path], false),
    };
    words.extend(args);
    (words, in_pane.unwrap_or(!gui))
}

fn base_name(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._+-:=@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> FileTarget {
        FileTarget {
            path: PathBuf::from("/src/my app/main.rs"),
            line: 42,
            col: Some(7),
        }
    }

    #[test]
    fn templates_fill_placeholders() {
        let (argv, pane) = editor_command(Some("nvim +{line} {file}"), None, &target());
        assert_eq!(argv, vec!["nvim", "+42", "/src/my app/main.rs"]);
        assert!(pane);
        let (argv, pane) = editor_command(Some("code -g {file}:{line}:{col}"), None, &target());
        assert_eq!(argv[2], "/src/my app/main.rs:42:7");
        assert!(!pane, "GUI editors get their own window");
        let (_, pane) = editor_command(Some("code -g {file}"), Some(true), &target());
        assert!(pane, "explicit editor_in_pane wins");
    }

    #[test]
    fn quoting_keeps_spaces_and_quotes_safe() {
        assert_eq!(shell_quote("/a/b.rs"), "/a/b.rs");
        assert_eq!(shell_quote("/my app/x"), "'/my app/x'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
