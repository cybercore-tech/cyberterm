// src/layout_file.rs
//
// Layouts as code: a project's `.cyberterm/layout.toml` describes its
// tabs, their splits and what runs in each pane, and `cyberterm +layout`
// (or the `load_layout` control method) opens the whole workspace at once.
//
//     [[tab]]
//     title = "dev"
//     [tab.layout]
//     split = "right"
//     ratio = 0.6
//     [[tab.layout.panes]]
//     command = "nvim ."
//     focus = true
//     [[tab.layout.panes]]
//     split = "down"
//     panes = [{ command = "cargo watch -x test" }, { command = "just serve", cwd = "web" }]
//
// Relative `cwd`s resolve against the enclosing tab's `cwd`, which resolves
// against the layout file's project directory. Commands are typed into a
// shell once it's ready, so the shell stays after the command exits.
//
// Parsing and planning are pure; the app turns a `Plan` into sessions.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::layout::Axis;

pub const DEFAULT_PATH: &str = ".cyberterm/layout.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutFile {
    #[serde(rename = "tab")]
    pub tabs: Vec<TabSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSpec {
    pub title: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub layout: PaneSpec,
}

/// A pane (with `command`/`cwd`) or a split of panes (`split` + `panes`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneSpec {
    pub split: Option<String>,
    pub ratio: Option<f32>,
    #[serde(default)]
    pub panes: Vec<PaneSpec>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

/// One tab, ready to spawn.
#[derive(Debug, PartialEq)]
pub struct TabPlan {
    pub title: Option<String>,
    pub root: Plan,
}

#[derive(Debug, PartialEq)]
pub enum Plan {
    Pane {
        cwd: PathBuf,
        command: Option<String>,
        focus: bool,
    },
    Split {
        axis: Axis,
        ratio: f32,
        a: Box<Plan>,
        b: Box<Plan>,
    },
}

/// Reads and plans a layout file. `path` may be the file or a project
/// directory containing `.cyberterm/layout.toml`.
pub fn load(path: &Path) -> Result<Vec<TabPlan>, String> {
    let file = if path.is_dir() {
        path.join(DEFAULT_PATH)
    } else {
        path.to_path_buf()
    };
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let parsed: LayoutFile =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let project = project_dir(&file);
    plan(&parsed, &project).map_err(|e| format!("{}: {e}", file.display()))
}

/// `/proj/.cyberterm/layout.toml` -> `/proj`; any other file -> its dir.
fn project_dir(file: &Path) -> PathBuf {
    let dir = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    if dir.file_name().is_some_and(|n| n == ".cyberterm") {
        dir.parent().map(Path::to_path_buf).unwrap_or(dir)
    } else {
        dir
    }
}

fn resolve(base: &Path, cwd: Option<&str>) -> PathBuf {
    match cwd {
        None => base.to_path_buf(),
        Some(c) if c.starts_with("~/") => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(&c[2..]))
            .unwrap_or_else(|| base.join(c)),
        Some(c) => base.join(c),
    }
}

pub fn plan(file: &LayoutFile, project: &Path) -> Result<Vec<TabPlan>, String> {
    if file.tabs.is_empty() {
        return Err("a layout needs at least one [[tab]]".into());
    }
    file.tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let base = resolve(project, tab.cwd.as_deref());
            Ok(TabPlan {
                title: tab.title.clone(),
                root: plan_pane(&tab.layout, &base).map_err(|e| format!("tab {}: {e}", i + 1))?,
            })
        })
        .collect()
}

fn plan_pane(spec: &PaneSpec, base: &Path) -> Result<Plan, String> {
    let Some(split) = &spec.split else {
        if !spec.panes.is_empty() {
            return Err("`panes` needs `split = \"right\"` or `\"down\"`".into());
        }
        return Ok(Plan::Pane {
            cwd: resolve(base, spec.cwd.as_deref()),
            command: spec.command.clone().filter(|c| !c.trim().is_empty()),
            focus: spec.focus,
        });
    };
    let axis = match split.as_str() {
        "right" => Axis::Horizontal,
        "down" => Axis::Vertical,
        other => {
            return Err(format!(
                "split must be \"right\" or \"down\", not \"{other}\""
            ))
        }
    };
    if spec.command.is_some() {
        return Err("a split can't have a `command`; put it on one of its panes".into());
    }
    if spec.panes.len() < 2 {
        return Err("a split needs at least two `panes`".into());
    }
    // Children of a split inherit its cwd.
    let base = resolve(base, spec.cwd.as_deref());
    let children = spec
        .panes
        .iter()
        .map(|p| plan_pane(p, &base))
        .collect::<Result<Vec<_>, _>>()?;
    // N panes become a chain of binary splits sharing space evenly; an
    // explicit ratio applies to the first pane.
    let n = children.len();
    let mut iter = children.into_iter().rev();
    let mut node = iter.next().expect("at least two panes");
    for (k, child) in iter.enumerate() {
        let count = k + 2; // panes in the node being built
        let even = 1.0 / count as f32;
        let ratio = if count == n {
            spec.ratio.unwrap_or(even)
        } else {
            even
        };
        node = Plan::Split {
            axis,
            ratio: ratio.clamp(0.05, 0.95),
            a: Box::new(child),
            b: Box::new(node),
        };
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<TabPlan>, String> {
        let file: LayoutFile = toml::from_str(text).map_err(|e| e.to_string())?;
        plan(&file, Path::new("/proj"))
    }

    #[test]
    fn plain_tab_is_one_shell_in_the_project() {
        let plans = parse("[[tab]]\ntitle = \"shell\"\n").unwrap();
        assert_eq!(
            plans,
            vec![TabPlan {
                title: Some("shell".into()),
                root: Plan::Pane {
                    cwd: PathBuf::from("/proj"),
                    command: None,
                    focus: false
                }
            }]
        );
    }

    #[test]
    fn nested_splits_with_relative_cwds() {
        let plans = parse(
            r#"
[[tab]]
cwd = "app"
[tab.layout]
split = "right"
ratio = 0.6
panes = [
  { command = "nvim .", focus = true },
  { split = "down", cwd = "web", panes = [ { command = "npm test" }, { command = "npm run dev", cwd = "ui" } ] },
]
"#,
        )
        .unwrap();
        let Plan::Split { axis, ratio, a, b } = &plans[0].root else {
            panic!("expected a split")
        };
        assert_eq!((*axis, *ratio), (Axis::Horizontal, 0.6));
        assert_eq!(
            **a,
            Plan::Pane {
                cwd: PathBuf::from("/proj/app"),
                command: Some("nvim .".into()),
                focus: true
            }
        );
        let Plan::Split {
            axis, b: inner_b, ..
        } = &**b
        else {
            panic!("expected the inner split")
        };
        assert_eq!(*axis, Axis::Vertical);
        assert_eq!(
            **inner_b,
            Plan::Pane {
                cwd: PathBuf::from("/proj/app/web/ui"),
                command: Some("npm run dev".into()),
                focus: false
            }
        );
    }

    #[test]
    fn three_panes_share_space_evenly() {
        let plans = parse(
            "[[tab]]\n[tab.layout]\nsplit = \"down\"\npanes = [{command = \"a\"}, {command = \"b\"}, {command = \"c\"}]\n",
        )
        .unwrap();
        let Plan::Split { ratio, b, .. } = &plans[0].root else {
            panic!()
        };
        assert!((ratio - 1.0 / 3.0).abs() < 1e-6);
        let Plan::Split { ratio: inner, .. } = &**b else {
            panic!()
        };
        assert_eq!(*inner, 0.5);
    }

    #[test]
    fn mistakes_are_reported() {
        assert!(parse("").unwrap_err().contains("missing field"));
        assert!(parse("tab = []").unwrap_err().contains("at least one"));
        assert!(
            parse("[[tab]]\n[tab.layout]\nsplit = \"sideways\"\npanes = [{}, {}]\n")
                .unwrap_err()
                .contains("right")
        );
        assert!(
            parse("[[tab]]\n[tab.layout]\nsplit = \"right\"\npanes = [{}]\n")
                .unwrap_err()
                .contains("two")
        );
        assert!(parse("[[tab]]\n[tab.layout]\npanes = [{}, {}]\n")
            .unwrap_err()
            .contains("needs `split"));
        assert!(parse("[[tab]]\nbogus = 1\n").is_err());
    }

    #[test]
    fn project_dir_strips_the_dot_cyberterm_folder() {
        assert_eq!(
            project_dir(Path::new("/nonexistent/proj/.cyberterm/layout.toml")),
            PathBuf::from("/nonexistent/proj")
        );
        assert_eq!(
            project_dir(Path::new("/nonexistent/elsewhere/dev.toml")),
            PathBuf::from("/nonexistent/elsewhere")
        );
    }
}
