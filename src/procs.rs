// src/procs.rs
//
// What's running in a pane, from /proc: the foreground process of the
// pane's terminal (read from the shell's `tpgid`, so it works for daemon
// panes too, whose PTY lives in another process), its command line and
// user, and -- for SSH -- the destination it connects to.
//
// Used by danger mode (is this pane on a production host / root?) and
// SSH-aware splits (open the new pane on the same host).

use std::collections::HashMap;
use std::path::Path;

/// The foreground process group leader of the terminal `shell_pid` is
/// attached to: the shell itself at a prompt, or the program it's running.
pub fn foreground(shell_pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{shell_pid}/stat")).ok()?;
    // Fields after the command name (which may contain spaces and parens):
    // state ppid pgrp session tty_nr tpgid ...
    let rest = &stat[stat.rfind(')')? + 1..];
    let tpgid: i64 = rest.split_whitespace().nth(5)?.parse().ok()?;
    (tpgid > 0).then_some(tpgid as u32)
}

pub fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The effective uid of a process.
pub fn euid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("Uid:"))?
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// The program name of argv (`/usr/bin/ssh` -> `ssh`).
pub fn program(args: &[String]) -> &str {
    args.first()
        .map(|a| a.rsplit('/').next().unwrap_or(a))
        .unwrap_or_default()
}

/// An `ssh` invocation: where it goes and the options to reuse to get
/// there again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    /// The destination as typed (`host`, `user@host`, `ssh://...`).
    pub destination: String,
    /// The host name alone, for matching against patterns.
    pub host: String,
    /// The options before the destination (`-p 2222`, `-i key`, `-J jump`).
    pub options: Vec<String>,
    /// A remote command was given (`ssh host make`), so this isn't an
    /// interactive login.
    pub has_command: bool,
}

/// ssh options that take a value (from ssh(1)).
const SSH_VALUE_OPTS: &str = "BbcDEeFIiJLlmopRSWw";

/// Parses ssh's argv (argv[0] included). None for anything that isn't a
/// plain connection (`ssh -V`, `ssh -O check`, missing destination).
pub fn ssh_target(args: &[String]) -> Option<SshTarget> {
    if program(args) != "ssh" {
        return None;
    }
    let mut options = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        options.push(a.clone());
        // Flags cluster (`-tA`); the first one that takes a value takes the
        // rest of the word (`-p2222`) or else the next argument (`-p 2222`).
        for (idx, c) in a[1..].char_indices() {
            if matches!(c, 'V' | 'G' | 'O' | 'Q') {
                // Version, config dump, mux control, queries: no session.
                return None;
            }
            if SSH_VALUE_OPTS.contains(c) {
                if idx + c.len_utf8() == a.len() - 1 {
                    if let Some(v) = args.get(i + 1) {
                        options.push(v.clone());
                        i += 1;
                    }
                }
                break;
            }
        }
        i += 1;
    }
    let destination = args.get(i)?.clone();
    let host_part = destination
        .strip_prefix("ssh://")
        .unwrap_or(&destination)
        .rsplit('@')
        .next()
        .unwrap_or_default();
    // `host:port` only appears in the ssh:// form; IPv6 is bracketed.
    let host = if destination.starts_with("ssh://") {
        match host_part.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or_default().to_string(),
            None => host_part
                .split([':', '/'])
                .next()
                .unwrap_or_default()
                .to_string(),
        }
    } else {
        host_part.to_string()
    };
    if host.is_empty() {
        return None;
    }
    Some(SshTarget {
        destination,
        host,
        options,
        has_command: args.len() > i + 1,
    })
}

/// Every process's parent, for walking a pane's process tree: one scan of
/// /proc, reused for all panes in a refresh.
pub fn parents() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let ppid = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().nth(1))
            .and_then(|p| p.parse().ok());
        if let Some(ppid) = ppid {
            map.insert(pid, ppid);
        }
    }
    map
}

/// `root` and everything below it.
pub fn tree(root: u32, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in parents {
        children.entry(ppid).or_default().push(pid);
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        if let Some(kids) = children.get(&out[i]) {
            out.extend(kids.iter().copied().filter(|k| *k != root));
        }
        i += 1;
        if out.len() > 10_000 {
            break;
        }
    }
    out
}

/// Listening TCP sockets: inode -> port (IPv4 and IPv6).
pub fn listening() -> HashMap<u64, u16> {
    let mut map = HashMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(file) {
            map.extend(text.lines().skip(1).filter_map(parse_listen_line));
        }
    }
    map
}

/// One /proc/net/tcp row -> (inode, port) if it's listening (state 0A).
fn parse_listen_line(line: &str) -> Option<(u64, u16)> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.get(3) != Some(&"0A") {
        return None;
    }
    let port = u16::from_str_radix(cols.get(1)?.rsplit(':').next()?, 16).ok()?;
    let inode = cols.get(9)?.parse().ok()?;
    Some((inode, port))
}

/// The socket inodes a process has open.
pub fn socket_inodes(pid: u32) -> Vec<u64> {
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter_map(|target| {
            target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect()
}

/// TCP ports that processes under `shell` are listening on, sorted. The
/// shell itself is skipped (it doesn't serve anything).
pub fn ports_under(
    shell: u32,
    parents: &HashMap<u32, u32>,
    listening: &HashMap<u64, u16>,
) -> Vec<u16> {
    if listening.is_empty() {
        return Vec::new();
    }
    let mut ports: Vec<u16> = tree(shell, parents)
        .into_iter()
        .skip(1)
        .flat_map(socket_inodes)
        .filter_map(|inode| listening.get(&inode).copied())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Shell-style wildcard match (`*`, `?`), case-insensitive.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// `glob` for paths, with `~/` expanded.
pub fn path_glob(pattern: &str, path: &Path) -> bool {
    let expanded = match pattern.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => pattern.to_string(),
        },
        None => pattern.to_string(),
    };
    glob(&expanded, &path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn ssh_destinations_and_options() {
        let t = ssh_target(&args("ssh prod-db")).unwrap();
        assert_eq!((t.host.as_str(), t.has_command), ("prod-db", false));
        let t = ssh_target(&args(
            "/usr/bin/ssh -p 2222 -i ~/.ssh/k deploy@web1.prod.example.com",
        ))
        .unwrap();
        assert_eq!(t.host, "web1.prod.example.com");
        assert_eq!(t.destination, "deploy@web1.prod.example.com");
        assert_eq!(t.options, args("-p 2222 -i ~/.ssh/k"));
        let t = ssh_target(&args("ssh -tA -J bastion box uptime")).unwrap();
        assert_eq!(t.host, "box");
        assert_eq!(t.options, args("-tA -J bastion"));
        assert!(t.has_command);
        let t = ssh_target(&args("ssh -p2222 -oStrictHostKeyChecking=no h")).unwrap();
        assert_eq!(t.options, args("-p2222 -oStrictHostKeyChecking=no"));
        assert_eq!(
            ssh_target(&args("ssh ssh://me@[::1]:22")).unwrap().host,
            "::1"
        );
        assert!(ssh_target(&args("ssh -V")).is_none());
        assert!(ssh_target(&args("ssh -O check host")).is_none());
        assert!(ssh_target(&args("ssh")).is_none());
        assert!(ssh_target(&args("mosh host")).is_none());
    }

    #[test]
    fn listen_lines_parse() {
        let l = "   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0 100 0 0 10 0";
        assert_eq!(parse_listen_line(l), Some((4242, 3000)));
        let established = l.replace(" 0A ", " 01 ");
        assert_eq!(parse_listen_line(&established), None);
    }

    #[test]
    fn finds_a_port_this_process_listens_on() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        let open = listening();
        assert!(socket_inodes(me).iter().any(|i| open.get(i) == Some(&port)));
        // Under our parent, we're a descendant that listens.
        let parents = parents();
        let parent = parents[&me];
        assert!(ports_under(parent, &parents, &open).contains(&port));
    }

    #[test]
    fn globs() {
        assert!(glob("*prod*", "web1.PROD.example"));
        assert!(glob("db-?", "db-1"));
        assert!(!glob("db-?", "db-10"));
        assert!(glob("*", ""));
        assert!(!glob("prod", "production"));
        assert!(glob("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn this_process_has_a_command_line_and_uid() {
        let me = std::process::id();
        assert!(!cmdline(me).is_empty());
        assert!(euid(me).is_some());
    }
}
