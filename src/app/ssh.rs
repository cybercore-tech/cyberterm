// src/app/ssh.rs
//
// SSH-aware splits: splitting a pane that's in an interactive `ssh`
// session opens the new pane on the same host. The new pane gets a local
// shell with the same `ssh` command typed into it (so it's in history and
// you land back in a local shell when it ends), plus `cd` into the remote
// directory when the remote shell reports it (OSC 7 with its host name).
//
// One connection per pane means one more login; an SSH ControlMaster in
// ~/.ssh/config makes the new panes reuse the first connection.

use std::path::Path;

use super::*;
use crate::layout::Direction;
use crate::procs;

impl App {
    /// The command that reconnects the focused pane's SSH session, if the
    /// pane is in an interactive one.
    pub(super) fn ssh_follow_command(&self) -> Option<String> {
        let pane = self.focused_pane()?;
        let shell = pane.session.pid();
        let fg = procs::foreground(shell).filter(|&fg| fg != shell)?;
        let target = procs::ssh_target(&procs::cmdline(fg))?;
        let remote_dir = pane
            .session
            .shell
            .lock()
            .remote_cwd
            .clone()
            .map(|(_, dir)| dir);
        follow_command(&target, remote_dir.as_deref())
    }

    /// The split action: on the same host when the pane is in SSH (and
    /// `[splits] follow_ssh` is on), else a local shell in the same directory.
    pub(super) fn split(&mut self, dir: Direction) {
        let follow = if self.config.splits.follow_ssh {
            self.ssh_follow_command()
        } else {
            None
        };
        let cwd = self.inherited_cwd();
        match self.split_with(dir, cwd) {
            Ok(id) => self.run_in(id, follow.as_deref()),
            Err(e) => eprintln!("cyberterm: couldn't split: {e}"),
        }
    }

    /// A split with a local shell, even from an SSH pane (context menu).
    pub(super) fn split_local(&mut self, dir: Direction) {
        let cwd = self.inherited_cwd();
        if let Err(e) = self.split_with(dir, cwd) {
            eprintln!("cyberterm: couldn't split: {e}");
        }
    }
}

/// `ssh <same options> <destination>`, with `-t … cd <dir> && exec $SHELL
/// -l` when the remote directory is known. None for sessions that aren't
/// interactive logins (a remote command, `-N` forwarding, `-f`, `-W`).
fn follow_command(target: &procs::SshTarget, remote_dir: Option<&Path>) -> Option<String> {
    if target.has_command {
        return None;
    }
    let flags_with = |set: &str| {
        target.options.iter().any(|o| {
            o.starts_with('-') && !o.starts_with("--") && o[1..].chars().any(|c| set.contains(c))
        })
    };
    if flags_with("NfW") {
        return None;
    }
    let mut parts: Vec<String> = vec!["ssh".into()];
    parts.extend(target.options.iter().map(|o| shell_quote(o)));
    match remote_dir {
        Some(dir) => {
            if !flags_with("t") {
                parts.push("-t".into());
            }
            parts.push(shell_quote(&target.destination));
            // One argument for the remote shell, quoted for ours.
            let remote = format!(
                "cd {} && exec \"$SHELL\" -l",
                shell_quote(&dir.to_string_lossy())
            );
            parts.push(shell_quote(&remote));
        }
        None => parts.push(shell_quote(&target.destination)),
    }
    Some(parts.join(" "))
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-:=@,%~".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(argv: &str) -> procs::SshTarget {
        let args: Vec<String> = argv.split_whitespace().map(str::to_string).collect();
        procs::ssh_target(&args).unwrap()
    }

    #[test]
    fn reconnects_with_the_same_options() {
        assert_eq!(
            follow_command(&target("ssh -p 2222 -i ~/.ssh/k me@box"), None).unwrap(),
            "ssh -p 2222 -i ~/.ssh/k me@box"
        );
    }

    #[test]
    fn lands_in_the_remote_directory() {
        let cmd = follow_command(&target("ssh box"), Some(Path::new("/srv/my app"))).unwrap();
        assert_eq!(
            cmd,
            r#"ssh -t box 'cd '\''/srv/my app'\'' && exec "$SHELL" -l'"#
        );
        // Run through a real shell, the remote command is one argument.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {}; printf '%s\\n' \"$3\"", &cmd[4..]))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            r#"cd '/srv/my app' && exec "$SHELL" -l"#
        );
    }

    #[test]
    fn skips_non_interactive_sessions() {
        assert!(follow_command(&target("ssh box uptime"), None).is_none());
        assert!(follow_command(&target("ssh -N -L 8080:localhost:80 box"), None).is_none());
        assert!(follow_command(&target("ssh -fN box"), None).is_none());
        assert!(
            follow_command(&target("ssh -tt box"), Some(Path::new("/x")))
                .is_some_and(|c| !c.contains(" -t box"))
        );
    }
}
