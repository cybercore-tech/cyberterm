// src/mux/layout_doc.rs
//
// A session's tabs and splits as stored in the daemon (`SaveLayout`), shared
// by the window and the terminal attach client so either can restore what
// the other left.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::layout::{Node, Removed, Tab, TabId};
use crate::session::PaneId;

#[derive(Serialize, Deserialize)]
struct LayoutDoc {
    tabs: Vec<TabDoc>,
    active: usize,
}

#[derive(Serialize, Deserialize)]
struct TabDoc {
    title: Option<String>,
    root: Node,
    focused: PaneId,
    zoomed: bool,
    broadcast: bool,
}

pub fn encode(tabs: &[Tab], active: usize) -> Value {
    let doc = LayoutDoc {
        tabs: tabs
            .iter()
            .map(|t| TabDoc {
                title: t.title.clone(),
                root: t.root.clone(),
                focused: t.focused,
                zoomed: t.zoomed,
                broadcast: t.broadcast,
            })
            .collect(),
        active,
    };
    serde_json::to_value(doc).unwrap_or(Value::Null)
}

/// Rebuilds tabs for the panes that still exist (`panes`, in session
/// order). Panes gone since the layout was saved are dropped from it;
/// panes it doesn't mention get a tab each. Returns the tabs and the
/// active tab index; tab ids come from `next_tab`.
pub fn decode(layout: &Value, panes: &[PaneId], next_tab: &mut TabId) -> (Vec<Tab>, usize) {
    let mut tabs = Vec::new();
    let mut placed: Vec<PaneId> = Vec::new();
    let mut active = 0;
    if let Ok(doc) = serde_json::from_value::<LayoutDoc>(layout.clone()) {
        active = doc.active;
        for tab in doc.tabs {
            let mut root = tab.root;
            let mut alive = true;
            for id in root.panes() {
                if !panes.contains(&id) || placed.contains(&id) {
                    if let Removed::Empty = root.remove(id) {
                        alive = false;
                        break;
                    }
                }
            }
            if !alive {
                continue;
            }
            let ids = root.panes();
            placed.extend(ids.iter().copied());
            let focused = if ids.contains(&tab.focused) {
                tab.focused
            } else {
                ids[0]
            };
            let mut t = Tab::new(*next_tab, focused);
            *next_tab += 1;
            t.root = root;
            t.title = tab.title;
            t.zoomed = tab.zoomed && ids.len() > 1;
            t.broadcast = tab.broadcast && ids.len() > 1;
            tabs.push(t);
        }
    }
    for id in panes.iter().filter(|id| !placed.contains(id)) {
        tabs.push(Tab::new(*next_tab, *id));
        *next_tab += 1;
    }
    let active = active.min(tabs.len().saturating_sub(1));
    (tabs, active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Axis;

    #[test]
    fn round_trips_and_prunes_missing_panes() {
        let mut t1 = Tab::new(0, 1);
        t1.root.split(1, Axis::Horizontal, 2, false);
        t1.root.split(2, Axis::Vertical, 3, false);
        t1.focused = 3;
        t1.title = Some("dev".into());
        let t2 = Tab::new(1, 4);
        let saved = encode(&[t1, t2], 1);

        let mut next = 10;
        let (tabs, active) = decode(&saved, &[1, 2, 3, 4], &mut next);
        assert_eq!(tabs.len(), 2);
        assert_eq!(active, 1);
        assert_eq!(tabs[0].root.panes(), vec![1, 2, 3]);
        assert_eq!(tabs[0].focused, 3);
        assert_eq!(tabs[0].title.as_deref(), Some("dev"));
        assert_eq!((tabs[0].id, tabs[1].id, next), (10, 11, 12));

        // Pane 3 exited and pane 9 is new since the save.
        let (tabs, _) = decode(&saved, &[1, 2, 4, 9], &mut next);
        assert_eq!(tabs[0].root.panes(), vec![1, 2]);
        assert_eq!(tabs[0].focused, 1);
        assert_eq!(tabs.last().unwrap().root.panes(), vec![9]);

        // A tab whose panes are all gone disappears; active is clamped.
        let (tabs, active) = decode(&saved, &[1, 2, 3], &mut next);
        assert_eq!(tabs.len(), 1);
        assert_eq!(active, 0);
    }

    #[test]
    fn no_layout_means_a_tab_per_pane() {
        let mut next = 0;
        let (tabs, active) = decode(&Value::Null, &[5, 6], &mut next);
        assert_eq!(
            tabs.iter().map(|t| t.focused).collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(active, 0);
        let (tabs, _) = decode(&Value::Null, &[], &mut next);
        assert!(tabs.is_empty());
    }
}
