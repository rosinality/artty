//! Split layouts: a binary tree of panes, and its geometry.

use crate::SessionId;

pub type PaneId = SessionId;

/// How a split arranges its two children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Axis {
    /// Side by side, with a vertical divider.
    Horizontal,
    /// Stacked, with a horizontal divider.
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// A rectangle in window pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn right(&self) -> f32 {
        self.x + self.width
    }

    fn bottom(&self) -> f32 {
        self.y + self.height
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Node {
    Pane(PaneId),
    Split {
        axis: Axis,
        /// Share of the space given to `first`, 0–1.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// A divider between two children, for drawing and dragging.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// Path from the root to the split: `false` = first child, `true` = second.
    pub path: Vec<bool>,
    pub axis: Axis,
    /// The gap between the children.
    pub rect: Rect,
    /// The area the split divides, for turning a drag position into a ratio.
    pub area: Rect,
}

pub const MIN_RATIO: f32 = 0.1;

impl Node {
    /// Panes in reading order (left to right, top to bottom).
    pub fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.collect(&mut panes);
        panes
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Pane(id) => out.push(*id),
            Node::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            },
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Pane(pane) => *pane == id,
            Node::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// Split `target`, putting `new` after it (right or below). Returns
    /// whether `target` was found.
    pub fn split(&mut self, target: PaneId, new: PaneId, axis: Axis) -> bool {
        match self {
            Node::Pane(id) if *id == target => {
                *self = Node::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(target)),
                    second: Box::new(Node::Pane(new)),
                };
                true
            },
            Node::Pane(_) => false,
            Node::Split { first, second, .. } => {
                first.split(target, new, axis) || second.split(target, new, axis)
            },
        }
    }

    /// The tree without `target`: its sibling takes the parent's place.
    /// `None` if `target` was the only pane.
    pub fn without(self, target: PaneId) -> Option<Node> {
        match self {
            Node::Pane(id) if id == target => None,
            Node::Pane(_) => Some(self),
            Node::Split { axis, ratio, first, second } => {
                match (first.without(target), second.without(target)) {
                    (Some(first), Some(second)) => Some(Node::Split {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            },
        }
    }

    pub fn set_ratio(&mut self, path: &[bool], value: f32) {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => *ratio = value.clamp(MIN_RATIO, 1. - MIN_RATIO),
            (Node::Split { first, second, .. }, Some((&go_second, rest))) => {
                if go_second {
                    second.set_ratio(rest, value)
                } else {
                    first.set_ratio(rest, value)
                }
            },
            (Node::Pane(_), _) => {},
        }
    }

    /// Pane rectangles and dividers for this tree filling `area`, with `gap`
    /// pixels between panes.
    pub fn layout(&self, area: Rect, gap: f32) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.layout_into(area, gap, &mut Vec::new(), &mut panes, &mut dividers);
        (panes, dividers)
    }

    fn layout_into(
        &self,
        area: Rect,
        gap: f32,
        path: &mut Vec<bool>,
        panes: &mut Vec<(PaneId, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Node::Pane(id) => panes.push((*id, area)),
            Node::Split { axis, ratio, first, second } => {
                let (a, divider, b) = match axis {
                    Axis::Horizontal => {
                        let first_width = ((area.width - gap) * ratio).round().max(0.);
                        let a = Rect { width: first_width, ..area };
                        let divider = Rect { x: area.x + first_width, width: gap, ..area };
                        let b = Rect {
                            x: divider.right(),
                            width: (area.width - first_width - gap).max(0.),
                            ..area
                        };
                        (a, divider, b)
                    },
                    Axis::Vertical => {
                        let first_height = ((area.height - gap) * ratio).round().max(0.);
                        let a = Rect { height: first_height, ..area };
                        let divider = Rect { y: area.y + first_height, height: gap, ..area };
                        let b = Rect {
                            y: divider.bottom(),
                            height: (area.height - first_height - gap).max(0.),
                            ..area
                        };
                        (a, divider, b)
                    },
                };
                dividers.push(Divider { path: path.clone(), axis: *axis, rect: divider, area });
                path.push(false);
                first.layout_into(a, gap, path, panes, dividers);
                path.pop();
                path.push(true);
                second.layout_into(b, gap, path, panes, dividers);
                path.pop();
            },
        }
    }
}

/// The ratio for a divider dragged to (`x`, `y`).
pub fn drag_ratio(divider: &Divider, x: f32, y: f32, gap: f32) -> f32 {
    let (position, start, size) = match divider.axis {
        Axis::Horizontal => (x, divider.area.x, divider.area.width),
        Axis::Vertical => (y, divider.area.y, divider.area.height),
    };
    ((position - start - gap / 2.) / (size - gap).max(1.)).clamp(MIN_RATIO, 1. - MIN_RATIO)
}

/// The pane next to `from` in `direction`: the closest one on that side,
/// preferring the one that overlaps `from` the most.
pub fn neighbor(panes: &[(PaneId, Rect)], from: PaneId, direction: Direction) -> Option<PaneId> {
    const EPSILON: f32 = 1.;
    let (_, origin) = panes.iter().find(|(id, _)| *id == from)?;
    let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| (a1.min(b1) - a0.max(b0)).max(0.);
    panes
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, rect)| {
            let (distance, shared) = match direction {
                Direction::Right if rect.x >= origin.right() - EPSILON => (
                    rect.x - origin.right(),
                    overlap(rect.y, rect.bottom(), origin.y, origin.bottom()),
                ),
                Direction::Left if rect.right() <= origin.x + EPSILON => (
                    origin.x - rect.right(),
                    overlap(rect.y, rect.bottom(), origin.y, origin.bottom()),
                ),
                Direction::Down if rect.y >= origin.bottom() - EPSILON => (
                    rect.y - origin.bottom(),
                    overlap(rect.x, rect.right(), origin.x, origin.right()),
                ),
                Direction::Up if rect.bottom() <= origin.y + EPSILON => (
                    origin.y - rect.bottom(),
                    overlap(rect.x, rect.right(), origin.x, origin.right()),
                ),
                _ => return None,
            };
            (shared > 0.).then_some((*id, distance, shared))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1).then(b.2.total_cmp(&a.2)))
        .map(|(id, _, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0., y: 0., width: 210., height: 110. };

    /// 1 | 2
    ///   | -
    ///   | 3
    fn three() -> Node {
        let mut root = Node::Pane(1);
        assert!(root.split(1, 2, Axis::Horizontal));
        assert!(root.split(2, 3, Axis::Vertical));
        root
    }

    #[test]
    fn splitting_and_order() {
        let root = three();
        assert_eq!(root.panes(), vec![1, 2, 3]);
        assert!(!root.clone().split(9, 4, Axis::Vertical));
    }

    #[test]
    fn geometry() {
        let (panes, dividers) = three().layout(AREA, 10.);
        assert_eq!(panes[0], (1, Rect { x: 0., y: 0., width: 100., height: 110. }));
        assert_eq!(panes[1], (2, Rect { x: 110., y: 0., width: 100., height: 50. }));
        assert_eq!(panes[2], (3, Rect { x: 110., y: 60., width: 100., height: 50. }));
        assert_eq!(dividers.len(), 2);
        assert_eq!(dividers[0].rect, Rect { x: 100., y: 0., width: 10., height: 110. });
        assert_eq!(dividers[1].path, vec![true]);
    }

    #[test]
    fn removing_promotes_the_sibling() {
        let root = three().without(2).unwrap();
        assert_eq!(
            root,
            Node::Split {
                axis: Axis::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Pane(1)),
                second: Box::new(Node::Pane(3)),
            }
        );
        assert_eq!(Node::Pane(1).without(1), None);
    }

    #[test]
    fn dragging_and_ratios() {
        let mut root = three();
        let (_, dividers) = root.layout(AREA, 10.);
        let ratio = drag_ratio(&dividers[0], 155., 0., 10.);
        assert_eq!(ratio, 0.75);
        root.set_ratio(&dividers[0].path, ratio);
        let (panes, _) = root.layout(AREA, 10.);
        assert_eq!(panes[0].1.width, 150.);
        // Can't collapse a pane entirely.
        root.set_ratio(&[], 0.);
        assert_eq!(root.layout(AREA, 10.).0[0].1.width, 20.);
    }

    #[test]
    fn neighbors() {
        let (panes, _) = three().layout(AREA, 10.);
        assert_eq!(neighbor(&panes, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&panes, 3, Direction::Left), Some(1));
        assert_eq!(neighbor(&panes, 2, Direction::Down), Some(3));
        assert_eq!(neighbor(&panes, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&panes, 1, Direction::Left), None);
        assert_eq!(neighbor(&panes, 2, Direction::Right), None);
    }
}
