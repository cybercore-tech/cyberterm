// src/layout.rs
//
// The split tree of one tab. A tab is a binary tree whose leaves are panes
// and whose inner nodes split their rectangle side by side or stacked at a
// ratio. Everything here is geometry on plain rectangles -- no sessions,
// no GPU -- so splitting, closing, resizing, directional focus and divider
// hit-testing are all unit-tested.

use serde::{Deserialize, Serialize};

use crate::renderer::Rect;
use crate::session::PaneId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Children side by side (a vertical divider between them).
    Horizontal,
    /// Children stacked (a horizontal divider between them).
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Horizontal,
            Direction::Up | Direction::Down => Axis::Vertical,
        }
    }
}

const MIN_RATIO: f32 = 0.05;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        /// Share of the space given to `a` (left or top).
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// A divider between two children, for drawing and mouse dragging.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// The gap between the two children.
    pub rect: Rect,
    pub axis: Axis,
    /// Path from the root to the split (false = a, true = b).
    pub path: Vec<bool>,
    /// The split's own rectangle, to turn a drag position into a ratio.
    pub parent: Rect,
}

fn split_rect(area: Rect, axis: Axis, ratio: f32, gap: f32) -> (Rect, Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let usable = (area.w - gap).max(0.0);
            let aw = (usable * ratio).round();
            let a = Rect { w: aw, ..area };
            let divider = Rect {
                x: area.x + aw,
                w: gap,
                ..area
            };
            let b = Rect {
                x: area.x + aw + gap,
                w: (usable - aw).max(0.0),
                ..area
            };
            (a, divider, b)
        }
        Axis::Vertical => {
            let usable = (area.h - gap).max(0.0);
            let ah = (usable * ratio).round();
            let a = Rect { h: ah, ..area };
            let divider = Rect {
                y: area.y + ah,
                h: gap,
                ..area
            };
            let b = Rect {
                y: area.y + ah + gap,
                h: (usable - ah).max(0.0),
                ..area
            };
            (a, divider, b)
        }
    }
}

pub enum Removed {
    /// The pane was not in this tree.
    NotFound,
    /// Removed; the tree still has panes.
    Done,
    /// The tree was just this pane and is now empty.
    Empty,
}

impl Node {
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(p) => *p == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// Splits the leaf `target`, putting `new` after it (right/below), or
    /// before it when `before` is set. Returns false if `target` isn't here.
    pub fn split(&mut self, target: PaneId, axis: Axis, new: PaneId, before: bool) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                let (a, b) = if before {
                    (Node::Leaf(new), Node::Leaf(target))
                } else {
                    (Node::Leaf(target), Node::Leaf(new))
                };
                *self = Node::Split {
                    axis,
                    ratio: 0.5,
                    a: Box::new(a),
                    b: Box::new(b),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                a.split(target, axis, new, before) || b.split(target, axis, new, before)
            }
        }
    }

    /// Removes a pane; its sibling takes over the parent split's space.
    pub fn remove(&mut self, id: PaneId) -> Removed {
        match self {
            Node::Leaf(p) if *p == id => Removed::Empty,
            Node::Leaf(_) => Removed::NotFound,
            Node::Split { a, b, .. } => {
                match a.remove(id) {
                    Removed::Empty => {
                        let sibling = (**b).clone();
                        *self = sibling;
                        return Removed::Done;
                    }
                    Removed::Done => return Removed::Done,
                    Removed::NotFound => {}
                }
                match b.remove(id) {
                    Removed::Empty => {
                        let sibling = (**a).clone();
                        *self = sibling;
                        Removed::Done
                    }
                    other => other,
                }
            }
        }
    }

    /// Pane rectangles inside `area`, with `gap` pixels between siblings.
    pub fn layout(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.layout_into(area, gap, &mut out);
        out
    }

    fn layout_into(&self, area: Rect, gap: f32, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, area)),
            Node::Split { axis, ratio, a, b } => {
                let (ra, _, rb) = split_rect(area, *axis, *ratio, gap);
                a.layout_into(ra, gap, out);
                b.layout_into(rb, gap, out);
            }
        }
    }

    pub fn dividers(&self, area: Rect, gap: f32) -> Vec<Divider> {
        let mut out = Vec::new();
        self.dividers_into(area, gap, &mut Vec::new(), &mut out);
        out
    }

    fn dividers_into(&self, area: Rect, gap: f32, path: &mut Vec<bool>, out: &mut Vec<Divider>) {
        if let Node::Split { axis, ratio, a, b } = self {
            let (ra, divider, rb) = split_rect(area, *axis, *ratio, gap);
            out.push(Divider {
                rect: divider,
                axis: *axis,
                path: path.clone(),
                parent: area,
            });
            path.push(false);
            a.dividers_into(ra, gap, path, out);
            path.pop();
            path.push(true);
            b.dividers_into(rb, gap, path, out);
            path.pop();
        }
    }

    pub fn set_ratio_at(&mut self, path: &[bool], value: f32) {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => *ratio = value.clamp(MIN_RATIO, 1.0 - MIN_RATIO),
            (Node::Split { a, b, .. }, Some((side, rest))) => {
                if *side { b } else { a }.set_ratio_at(rest, value)
            }
            _ => {}
        }
    }

    /// Moves the nearest divider on `dir`'s axis around pane `id` by
    /// `delta` (a fraction of that split's size) in direction `dir`.
    pub fn resize(&mut self, id: PaneId, dir: Direction, delta: f32) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { axis, ratio, a, b } => {
                let child = if a.contains(id) {
                    &mut **a
                } else if b.contains(id) {
                    &mut **b
                } else {
                    return false;
                };
                if child.resize(id, dir, delta) {
                    return true;
                }
                if *axis != dir.axis() {
                    return false;
                }
                let sign = match dir {
                    Direction::Right | Direction::Down => 1.0,
                    Direction::Left | Direction::Up => -1.0,
                };
                *ratio = (*ratio + sign * delta).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                true
            }
        }
    }

    /// Resets every split so leaves share space evenly along each axis.
    pub fn equalize(&mut self) {
        if let Node::Split { axis, ratio, a, b } = self {
            a.equalize();
            b.equalize();
            let weight = |n: &Node| n.count_along(*axis) as f32;
            *ratio = weight(a) / (weight(a) + weight(b));
        }
    }

    /// Leaves stacked along `axis` (so `a|b|c` counts 3 horizontally).
    fn count_along(&self, axis: Axis) -> usize {
        match self {
            Node::Leaf(_) => 1,
            Node::Split {
                axis: own, a, b, ..
            } if *own == axis => a.count_along(axis) + b.count_along(axis),
            Node::Split { a, b, .. } => a.count_along(axis).max(b.count_along(axis)),
        }
    }
}

/// The pane next to `from` in `dir`: among panes on that side whose span
/// overlaps `from`'s, the nearest, preferring the one most aligned.
pub fn neighbor(rects: &[(PaneId, Rect)], from: PaneId, dir: Direction) -> Option<PaneId> {
    let (_, f) = rects.iter().find(|(id, _)| *id == from)?;
    let eps = 0.5;
    rects
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, r)| {
            let (distance, overlap) = match dir {
                Direction::Right if r.x >= f.x + f.w - eps => {
                    (r.x - (f.x + f.w), overlap(f.y, f.h, r.y, r.h))
                }
                Direction::Left if r.x + r.w <= f.x + eps => {
                    (f.x - (r.x + r.w), overlap(f.y, f.h, r.y, r.h))
                }
                Direction::Down if r.y >= f.y + f.h - eps => {
                    (r.y - (f.y + f.h), overlap(f.x, f.w, r.x, r.w))
                }
                Direction::Up if r.y + r.h <= f.y + eps => {
                    (f.y - (r.y + r.h), overlap(f.x, f.w, r.x, r.w))
                }
                _ => return None,
            };
            (overlap > 0.0).then_some((*id, distance, overlap))
        })
        .min_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
        })
        .map(|(id, _, _)| id)
}

fn overlap(a0: f32, alen: f32, b0: f32, blen: f32) -> f32 {
    ((a0 + alen).min(b0 + blen) - a0.max(b0)).max(0.0)
}

pub type TabId = u32;

/// One tab: a split tree, which pane in it has focus, and tab state.
#[derive(Clone, Debug)]
pub struct Tab {
    pub id: TabId,
    pub root: Node,
    pub focused: PaneId,
    /// Show only the focused pane, full size.
    pub zoomed: bool,
    /// Typed input goes to every pane in the tab.
    pub broadcast: bool,
    /// A name set by the user or a layout; otherwise the focused pane's
    /// title is shown.
    pub title: Option<String>,
}

impl Tab {
    pub fn new(id: TabId, pane: PaneId) -> Self {
        Self {
            id,
            root: Node::Leaf(pane),
            focused: pane,
            zoomed: false,
            broadcast: false,
            title: None,
        }
    }

    /// Pane rectangles as currently shown (just the focused pane when
    /// zoomed).
    pub fn visible(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        if self.zoomed {
            vec![(self.focused, area)]
        } else {
            self.root.layout(area, gap)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 101.0,
        h: 51.0,
    };

    fn rect_of(rects: &[(PaneId, Rect)], id: PaneId) -> Rect {
        rects.iter().find(|(p, _)| *p == id).unwrap().1
    }

    #[test]
    fn split_right_then_down_tiles_the_area() {
        let mut root = Node::Leaf(1);
        assert!(root.split(1, Axis::Horizontal, 2, false));
        assert!(root.split(2, Axis::Vertical, 3, false));
        assert!(!root.split(9, Axis::Vertical, 4, false));
        assert_eq!(root.panes(), vec![1, 2, 3]);

        let rects = root.layout(AREA, 1.0);
        let (r1, r2, r3) = (rect_of(&rects, 1), rect_of(&rects, 2), rect_of(&rects, 3));
        assert_eq!((r1.x, r1.w, r1.h), (0.0, 50.0, 51.0));
        assert_eq!((r2.x, r2.y, r2.w, r2.h), (51.0, 0.0, 50.0, 25.0));
        assert_eq!((r3.x, r3.y, r3.h), (51.0, 26.0, 25.0));
    }

    #[test]
    fn split_before_puts_the_new_pane_first() {
        let mut root = Node::Leaf(1);
        root.split(1, Axis::Horizontal, 2, true);
        assert_eq!(root.panes(), vec![2, 1]);
    }

    #[test]
    fn removing_a_pane_gives_its_space_to_the_sibling() {
        let mut root = Node::Leaf(1);
        root.split(1, Axis::Horizontal, 2, false);
        root.split(2, Axis::Vertical, 3, false);
        assert!(matches!(root.remove(2), Removed::Done));
        assert_eq!(root.panes(), vec![1, 3]);
        let rects = root.layout(AREA, 1.0);
        assert_eq!(rect_of(&rects, 3).h, 51.0);
        assert!(matches!(root.remove(7), Removed::NotFound));
        assert!(matches!(root.remove(1), Removed::Done));
        assert_eq!(root, Node::Leaf(3));
        assert!(matches!(root.remove(3), Removed::Empty));
    }

    #[test]
    fn directional_neighbors() {
        let mut root = Node::Leaf(1);
        root.split(1, Axis::Horizontal, 2, false);
        root.split(2, Axis::Vertical, 3, false);
        let rects = root.layout(AREA, 1.0);
        assert_eq!(neighbor(&rects, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&rects, 3, Direction::Left), Some(1));
        assert_eq!(neighbor(&rects, 2, Direction::Down), Some(3));
        assert_eq!(neighbor(&rects, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&rects, 1, Direction::Left), None);
        assert_eq!(neighbor(&rects, 2, Direction::Up), None);
    }

    #[test]
    fn resize_moves_the_nearest_divider_on_that_axis() {
        let mut root = Node::Leaf(1);
        root.split(1, Axis::Horizontal, 2, false);
        root.split(2, Axis::Vertical, 3, false);
        // Pane 3 is in a vertical split, but moving right uses the outer
        // horizontal split.
        assert!(root.resize(3, Direction::Left, 0.1));
        let Node::Split { ratio, b, .. } = &root else {
            panic!()
        };
        assert!((ratio - 0.4).abs() < 1e-6);
        let Node::Split { ratio: inner, .. } = &**b else {
            panic!()
        };
        assert_eq!(*inner, 0.5);
        assert!(root.resize(3, Direction::Up, 0.2));
        // Ratios are clamped.
        root.resize(1, Direction::Left, 5.0);
        let Node::Split { ratio, .. } = &root else {
            panic!()
        };
        assert_eq!(*ratio, MIN_RATIO);
        assert!(!Node::Leaf(1).clone().resize(1, Direction::Left, 0.1));
    }

    #[test]
    fn equalize_spreads_three_columns_evenly() {
        let mut root = Node::Leaf(1);
        root.split(1, Axis::Horizontal, 2, false);
        root.split(2, Axis::Horizontal, 3, false);
        root.equalize();
        let rects = root.layout(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 300.0,
                h: 10.0,
            },
            0.0,
        );
        for id in [1, 2, 3] {
            assert_eq!(rect_of(&rects, id).w, 100.0);
        }
    }

    #[test]
    fn dividers_report_their_split_path() {
        let mut root = Node::Leaf(1);
        root.split(1, Axis::Horizontal, 2, false);
        root.split(2, Axis::Vertical, 3, false);
        let dividers = root.dividers(AREA, 1.0);
        assert_eq!(dividers.len(), 2);
        assert_eq!(dividers[0].axis, Axis::Horizontal);
        assert_eq!(dividers[0].rect.x, 50.0);
        assert_eq!(dividers[1].path, vec![true]);
        root.set_ratio_at(&[true], 0.75);
        let rects = root.layout(AREA, 1.0);
        assert_eq!(rect_of(&rects, 2).h, 38.0);
    }

    #[test]
    fn zoomed_tab_shows_only_the_focused_pane() {
        let mut tab = Tab::new(1, 1);
        tab.root.split(1, Axis::Horizontal, 2, false);
        tab.focused = 2;
        assert_eq!(tab.visible(AREA, 1.0).len(), 2);
        tab.zoomed = true;
        assert_eq!(tab.visible(AREA, 1.0), vec![(2, AREA)]);
    }
}
