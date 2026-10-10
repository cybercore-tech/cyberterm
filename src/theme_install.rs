// src/theme_install.rs
//
// `cyberterm +themes`: installs theme collections into the themes folder
// on request, instead of bundling them -- the binary stays small and each
// collection keeps its own license (saved next to it).
//
//   cyberterm +themes                     list collections and what's installed
//   cyberterm +themes install iterm2 ...  install (or update) collections; `all`
//   cyberterm +themes remove kitty        remove a collection
//
// A collection is a folder of a GitHub repository. One API call lists the
// repository's files; the theme files are then fetched in parallel from
// raw.githubusercontent.com into a temporary folder that replaces the old
// copy only when everything arrived.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

pub struct Collection {
    pub name: &'static str,
    pub title: &'static str,
    repo: &'static str,
    /// Git ref: a fixed one, or None for Cyberterm's own tag.
    git_ref: Option<&'static str>,
    prefix: &'static str,
    ext: &'static str,
    pub license: &'static str,
}

pub const COLLECTIONS: &[Collection] = &[
    Collection {
        name: "cyberterm",
        title: "Cyberterm extras: games, movies, music, sub-cyber",
        repo: "cybercore-tech/cyberterm",
        git_ref: None,
        prefix: "themes/",
        ext: ".json",
        license: "MIT",
    },
    Collection {
        name: "iterm2",
        title: "iTerm2-Color-Schemes (mbadolato), Kitty format",
        repo: "mbadolato/iTerm2-Color-Schemes",
        git_ref: Some("master"),
        prefix: "kitty/",
        ext: ".conf",
        license: "MIT",
    },
    Collection {
        name: "kitty",
        title: "kitty-themes (kovidgoyal)",
        repo: "kovidgoyal/kitty-themes",
        git_ref: Some("master"),
        prefix: "themes/",
        ext: ".conf",
        license: "GPL-3.0, downloaded for your own use",
    },
];

const PARALLEL: usize = 8;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn user_agent() -> String {
    format!("cyberterm/{}", env!("CARGO_PKG_VERSION"))
}

fn get_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    agent
        .get(url)
        .header("User-Agent", user_agent())
        .call()
        .map_err(|e| format!("{url}: {e}"))?
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("{url}: {e}"))
}

/// The theme files of a collection: (path in the repo, ref).
fn list(agent: &ureq::Agent, c: &Collection) -> Result<(Vec<String>, String), String> {
    let refs: Vec<String> = match c.git_ref {
        Some(r) => vec![r.to_string()],
        // Cyberterm's own themes as of this version, else the newest.
        None => vec![
            format!("v{}", env!("CARGO_PKG_VERSION")),
            "main".to_string(),
        ],
    };
    let mut last_err = String::new();
    for r in refs {
        let url = format!(
            "https://api.github.com/repos/{}/git/trees/{r}?recursive=1",
            c.repo
        );
        let text = match get_text(agent, &url) {
            Ok(t) => t,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        let tree: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let files = theme_paths(&tree, c.prefix, c.ext);
        if !files.is_empty() {
            return Ok((files, r));
        }
        last_err = format!("no themes under {} at {r}", c.prefix);
    }
    Err(last_err)
}

/// Theme file paths in a GitHub tree listing.
fn theme_paths(tree: &Value, prefix: &str, ext: &str) -> Vec<String> {
    tree["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "blob")
        .filter_map(|e| e["path"].as_str())
        .filter(|p| p.starts_with(prefix) && p.ends_with(ext))
        .filter(|p| !p.ends_with("theme_template.json"))
        .filter(|p| !p.split('/').any(|part| part == ".." || part.is_empty()))
        .map(str::to_string)
        .collect()
}

/// Installs (or refreshes) one collection; returns how many themes.
pub fn install(themes_dir: &Path, c: &Collection) -> Result<usize, String> {
    let agent = agent();
    let (files, git_ref) = list(&agent, c)?;
    let dest = themes_dir.join(c.name);
    let tmp = themes_dir.join(format!(".{}.downloading", c.name));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;

    let queue = Arc::new(Mutex::new(files.clone()));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let done = Arc::new(Mutex::new(0usize));
    let total = files.len();
    let workers: Vec<_> = (0..PARALLEL)
        .map(|_| {
            let (queue, errors, done, agent) =
                (queue.clone(), errors.clone(), done.clone(), agent.clone());
            let (repo, git_ref, prefix, tmp) = (c.repo, git_ref.clone(), c.prefix, tmp.clone());
            std::thread::spawn(move || loop {
                let Some(path) = queue.lock().unwrap().pop() else {
                    break;
                };
                let url = format!(
                    "https://raw.githubusercontent.com/{repo}/{git_ref}/{}",
                    encode_path(&path)
                );
                match get_text(&agent, &url) {
                    Ok(body) => {
                        let target = tmp.join(&path[prefix.len()..]);
                        if let Some(dir) = target.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        if let Err(e) = std::fs::write(&target, body) {
                            errors.lock().unwrap().push(format!("{path}: {e}"));
                        }
                    }
                    Err(e) => errors.lock().unwrap().push(e),
                }
                let mut d = done.lock().unwrap();
                *d += 1;
                if *d % 50 == 0 || *d == total {
                    eprint!("\r  {}: {}/{} ", c_name_for_progress(repo), *d, total);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    eprintln!();
    let errors = errors.lock().unwrap();
    if errors.len() * 10 > total {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "{} of {total} downloads failed (first: {})",
            errors.len(),
            errors.first().cloned().unwrap_or_default()
        ));
    }

    // The collection's license and where it came from.
    if let Ok(license) = get_text(
        &agent,
        &format!(
            "https://raw.githubusercontent.com/{}/{git_ref}/LICENSE",
            c.repo
        ),
    ) {
        let _ = std::fs::write(tmp.join("LICENSE"), license);
    }
    let source = format!(
        "{}\nhttps://github.com/{} @ {git_ref}\nlicense: {}\nthemes: {}\n",
        c.title,
        c.repo,
        c.license,
        total - errors.len()
    );
    let _ = std::fs::write(tmp.join("SOURCE"), source);

    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    Ok(total - errors.len())
}

fn c_name_for_progress(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Percent-encodes what raw URLs need encoded in a path.
fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Themes installed for a collection (counted from its folder).
pub fn installed(themes_dir: &Path, c: &Collection) -> usize {
    count_files(&themes_dir.join(c.name), c.ext)
}

fn count_files(dir: &Path, ext: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| e.path())
        .map(|p| {
            if p.is_dir() {
                count_files(&p, ext)
            } else {
                usize::from(p.to_string_lossy().ends_with(ext))
            }
        })
        .sum()
}

pub fn remove(themes_dir: &Path, c: &Collection) -> std::io::Result<bool> {
    let dir = themes_dir.join(c.name);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

/// `cyberterm +themes ...`. Returns the process exit code.
pub fn run(themes_dir: &Path, args: &[String]) -> i32 {
    let find = |name: &str| COLLECTIONS.iter().find(|c| c.name == name);
    match args.first().map(String::as_str) {
        None | Some("list") => {
            println!("Theme collections (cyberterm +themes install <name>... | all):\n");
            for c in COLLECTIONS {
                let n = installed(themes_dir, c);
                let state = if n > 0 {
                    format!("{n} installed")
                } else {
                    "not installed".into()
                };
                println!(
                    "  {:<10} {:<52} {:<15} {}",
                    c.name, c.title, state, c.license
                );
            }
            let registry = crate::theme::ThemeRegistry::load_from_dir(themes_dir);
            let mut registry = registry;
            registry.append_cybercore_themes();
            println!(
                "\n  {} themes available now (built-in, popular, the Cybercore catalog and installed collections).",
                registry.themes.len()
            );
            println!("  Folder: {}", themes_dir.display());
            0
        }
        Some("install") => {
            let names: Vec<&str> = if args[1..].iter().any(|a| a == "all") || args.len() == 1 {
                COLLECTIONS.iter().map(|c| c.name).collect()
            } else {
                args[1..].iter().map(String::as_str).collect()
            };
            let mut code = 0;
            for name in names {
                let Some(c) = find(name) else {
                    eprintln!("unknown collection `{name}` (try: cyberterm +themes)");
                    code = 2;
                    continue;
                };
                println!("Installing {} ({}) ...", c.name, c.license);
                match install(themes_dir, c) {
                    Ok(n) => println!("  {n} themes in {}", themes_dir.join(c.name).display()),
                    Err(e) => {
                        eprintln!("  failed: {e}");
                        code = 1;
                    }
                }
            }
            println!("Open the theme menu (Ctrl+Shift+,) to browse them.");
            code
        }
        Some("remove") => {
            let mut code = 0;
            for name in &args[1..] {
                match find(name).map(|c| remove(themes_dir, c)) {
                    Some(Ok(true)) => println!("Removed {name}."),
                    Some(Ok(false)) => println!("{name} isn't installed."),
                    Some(Err(e)) => {
                        eprintln!("{name}: {e}");
                        code = 1;
                    }
                    None => {
                        eprintln!("unknown collection `{name}`");
                        code = 2;
                    }
                }
            }
            code
        }
        Some(other) => {
            eprintln!("unknown +themes command `{other}` (list, install, remove)");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_theme_files_under_the_prefix_only() {
        let tree = json!({"tree": [
            {"path": "kitty/Dracula.conf", "type": "blob"},
            {"path": "kitty", "type": "tree"},
            {"path": "kitty/README.md", "type": "blob"},
            {"path": "alacritty/Dracula.toml", "type": "blob"},
            {"path": "themes/theme_template.json", "type": "blob"},
            {"path": "kitty/../evil.conf", "type": "blob"}
        ]});
        assert_eq!(
            theme_paths(&tree, "kitty/", ".conf"),
            vec!["kitty/Dracula.conf"]
        );
        assert!(theme_paths(&tree, "themes/", ".json").is_empty());
    }

    #[test]
    fn encodes_spaces_and_symbols_in_raw_urls() {
        assert_eq!(
            encode_path("kitty/Tokyo Night.conf"),
            "kitty/Tokyo%20Night.conf"
        );
        assert_eq!(encode_path("kitty/Dracula+.conf"), "kitty/Dracula%2B.conf");
    }

    #[test]
    fn counts_installed_files_recursively() {
        let dir = std::env::temp_dir().join(format!("ct-themes-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("cyberterm/music/metal")).unwrap();
        std::fs::write(dir.join("cyberterm/music/metal/a.json"), "{}").unwrap();
        std::fs::write(dir.join("cyberterm/SOURCE"), "x").unwrap();
        assert_eq!(installed(&dir, &COLLECTIONS[0]), 1);
        assert!(remove(&dir, &COLLECTIONS[0]).unwrap());
        assert_eq!(installed(&dir, &COLLECTIONS[0]), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
