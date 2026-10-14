//! A worktree's tabs, and the panes each one is made of.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::*;

use crate::resize::WidthDrag;

/// Which way a split divides. Core's type, because the tab layout is persisted
/// (`model::TabLayout`) and this is part of what it holds; the name here is what
/// the app calls it.
pub use syro_core::model::SplitAxis as Axis;

/// One pane's content: an id the workspace's pane map is keyed by. Stable for
/// the life of the content, which is what lets a close renumber nothing.
pub type Pane = usize;

/// Where a pane, or the divider above it, sits inside its tab: the turns taken
/// from the tab's root, `false` for a first child and `true` for a second. Empty
/// is the root itself.
pub type Path = Vec<bool>;

/// The frame's name for a pane, or for the divider of the split at `path`:
pub fn key(prefix: &str, path: &[bool]) -> String {
    let mut key = prefix.to_string();
    for side in path {
        key.push_str(if *side { "-1" } else { "-0" });
    }
    key
}

/// The divider's width in pixels. A flex item, so the panes and this add up to
/// the split exactly — which is what keeps a drag from changing the sum.
pub const DIVIDER: f32 = 6.;

/// The smallest a pane may be drawn, and what a divider starts at.
const DEFAULT_PANE: f32 = 560.;
pub const MIN_PANE: f32 = 220.;
const MAX_PANE: f32 = 1600.;

/// One node of a tab's pane tree.
pub enum Node {
    /// A pane: one view, named by its content.
    Pane { pane: Pane },
    /// Two subtrees with a divider between them. `size` is the first child's
    /// width (height, when stacked), dragged against fixed bounds; the second
    /// takes whatever is left.
    Split {
        axis: Axis,
        size: WidthDrag,
        /// How big this split's own box was when it was last drawn — its width, or its height when
        /// stacked.
        container: Rc<Cell<f32>>,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// One tab: its panes, and which of them has the keyboard. A path rather than a
/// pane, because a pane that closes takes its path with it and the pane beside
/// it keeps its own.
struct Tab {
    root: Node,
    focused: Path,
}

/// A worktree's tabs, in the bar's order, and which one is on screen.
pub struct Layout {
    tabs: Vec<Tab>,
    /// The tab the user is looking at: the bar marks it, and the actions that
    /// name no tab (cmd-d, cmd-w, cmd-t) mean this one.
    current: usize,
    /// The divider a drag is on — which tab, and where in it. Set by
    /// [`Layout::begin_drag`], cleared by [`Layout::end_drag`].
    dragging: Option<(usize, Path)>,
}

impl Default for Layout {
    /// No tabs at all. A worktree with nothing open is a worktree whose pane
    /// area is a message, and the first tab is made when its first pane is.
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            current: 0,
            dragging: None,
        }
    }
}

impl Layout {
    /// How many tabs the bar has.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// The tab on screen: the one the bar marks, and the one a split divides.
    pub fn current(&self) -> usize {
        self.current
    }

    /// The tree of one tab, for the frame to walk.
    pub fn root(&self, tab: usize) -> Option<&Node> {
        self.tabs.get(tab).map(|tab| &tab.root)
    }

    /// The panes of one tab, in draw order.
    pub fn panes_of(&self, tab: usize) -> Vec<Pane> {
        let mut panes = Vec::new();
        if let Some(tab) = self.tabs.get(tab) {
            collect_panes(&tab.root, &mut panes);
        }
        panes
    }

    /// Every pane of every tab, in the order the frame draws them.
    pub fn panes(&self) -> Vec<Pane> {
        let mut panes = Vec::new();
        for tab in &self.tabs {
            collect_panes(&tab.root, &mut panes);
        }
        panes
    }

    /// The pane with the keyboard, in the tab on screen.
    pub fn focused_pane(&self) -> Option<Pane> {
        let tab = self.tabs.get(self.current)?;
        leaf_pane(&tab.root, &tab.focused)
    }

    /// Where that pane sits in its tab, for the frame to name it and for the
    /// split to divide it.
    pub fn focused_path(&self) -> Path {
        self.tabs
            .get(self.current)
            .map(|tab| tab.focused.clone())
            .unwrap_or_default()
    }

    /// The pane with the keyboard in `tab`, or the tab's first pane when the one
    /// it remembers is gone. What the bar reads for the row's title.
    pub fn tab_focused_pane(&self, tab: usize) -> Option<Pane> {
        let entry = self.tabs.get(tab)?;
        leaf_pane(&entry.root, &entry.focused).or_else(|| self.panes_of(tab).first().copied())
    }

    /// Where a pane sits: its tab and its path in it.
    pub fn path_of_pane(&self, pane: Pane) -> Option<(usize, Path)> {
        for (index, tab) in self.tabs.iter().enumerate() {
            let mut path = Path::new();
            if path_of_pane(&tab.root, pane, &mut path) {
                return Some((index, path));
            }
        }
        None
    }

    /// The axis of a tab's outermost split: what the bar's mark says about the
    /// row — this tab is more than one pane, divided this way. The axis of a
    /// split *inside* a split is not a row's business.
    pub fn tab_axis(&self, tab: usize) -> Option<Axis> {
        match self.tabs.get(tab).map(|tab| &tab.root) {
            Some(Node::Split { axis, .. }) => Some(*axis),
            _ => None,
        }
    }

    /// Put `tab` on screen, keeping that tab's own pane focused. Returns whether
    /// the tab on screen changed, so a caller only redraws when it did.
    pub fn set_current(&mut self, tab: usize) -> bool {
        if tab >= self.tabs.len() || tab == self.current {
            return false;
        }
        self.current = tab;
        true
    }

    /// Give the keyboard to the pane at `path` in `tab`: what a click in a pane says, and what a
    /// right-click on its terminal says before the menu opens — which is how "split *this* one"
    /// knows which pane it means.
    pub fn focus(&mut self, tab: usize, path: &[bool]) -> bool {
        let Some(entry) = self.tabs.get_mut(tab) else {
            return false;
        };
        if leaf_pane(&entry.root, path).is_none() {
            return false;
        }
        let moved = self.current != tab || entry.focused != path;
        entry.focused = path.to_vec();
        self.current = tab;
        moved
    }

    /// Give the keyboard to the pane with this content. What the pane's own
    /// click says, without the element having to know its path.
    pub fn focus_pane(&mut self, pane: Pane) -> bool {
        let Some((tab, path)) = self.path_of_pane(pane) else {
            return false;
        };
        self.focus(tab, &path)
    }

    /// Give the keyboard to the first pane of `tab` — the leftmost of the topmost.
    pub fn focus_first(&mut self, tab: usize) {
        let Some(entry) = self.tabs.get_mut(tab) else {
            return;
        };
        let mut path = Path::new();
        first_path(&entry.root, &mut path);
        entry.focused = path;
    }

    /// A new tab holding `pane`, at the end of the bar, and on screen.
    pub fn add_tab(&mut self, pane: Pane) -> usize {
        self.tabs.push(Tab {
            root: Node::Pane { pane },
            focused: Path::new(),
        });
        self.current = self.tabs.len() - 1;
        self.current
    }

    /// Divide the pane at `path` in `tab` along `axis`, putting `pane` in the new half — which
    /// takes the keyboard, the way a split does everywhere.
    pub fn split(&mut self, tab: usize, path: &[bool], axis: Axis, pane: Pane) -> bool {
        let Some(entry) = self.tabs.get_mut(tab) else {
            return false;
        };
        let Some(node) = node_mut(&mut entry.root, path) else {
            return false;
        };
        let Node::Pane { pane: here } = *node else {
            return false;
        };
        *node = Node::Split {
            axis,
            size: WidthDrag::new(DEFAULT_PANE, MIN_PANE, MAX_PANE),
            container: Rc::new(Cell::new(0.)),
            first: Box::new(Node::Pane { pane: here }),
            second: Box::new(Node::Pane { pane }),
        };
        let mut focused = path.to_vec();
        focused.push(true);
        entry.focused = focused;
        self.current = tab;
        true
    }

    /// Close the pane at `path` in `tab`; its sibling takes the area it had.
    pub fn close_pane(&mut self, tab: usize, path: &[bool]) -> Option<(Pane, Option<usize>)> {
        let entry = self.tabs.get_mut(tab)?;
        let pane = leaf_pane(&entry.root, path)?;
        if path.is_empty() {
            // The tab's only pane: the tab goes with it.
            self.remove_tab(tab);
            return Some((pane, None));
        }

        let (side, parent) = path.split_last().expect("checked above");
        let node = node_mut(&mut entry.root, parent)?;
        if !collapse(node, *side) {
            return None;
        }
        // The tree collapsed: the sibling took the split's own place, so a path that went through the
        // half that *stayed* loses one turn, and one that went through the half that left
        // belongs to a pane that is gone — the keyboard goes to the sibling's first pane.
        if entry.focused.starts_with(path) {
            let mut first = parent.to_vec();
            let sibling = node_ref(&entry.root, &first).expect("the sibling is there");
            first_path(sibling, &mut first);
            entry.focused = first;
        } else if entry.focused.get(parent.len()) == Some(&!*side) {
            entry.focused.remove(parent.len());
        }
        Some((pane, Some(tab)))
    }

    /// Take a whole tab out of the bar, handing back every pane it held for the
    /// caller to drop. The tab on screen moves to the one that took its place.
    pub fn remove_tab(&mut self, tab: usize) -> Vec<Pane> {
        if tab >= self.tabs.len() {
            return Vec::new();
        }
        let gone = self.tabs.remove(tab);
        let mut panes = Vec::new();
        collect_panes(&gone.root, &mut panes);
        if self.current >= self.tabs.len() {
            self.current = self.tabs.len().saturating_sub(1);
        }
        // A drag that was on this tab's divider is a drag on a divider that is
        // gone.
        if self
            .dragging
            .as_ref()
            .is_some_and(|(drag_tab, _)| *drag_tab == tab)
        {
            self.dragging = None;
        }
        panes
    }

    /// Move the tab at `from` to `to` in the bar, the way a drag drops it. The
    /// tab on screen stays the same tab wherever it ends up.
    pub fn move_tab(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return false;
        }
        let current = self.current;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        let shift = |i: usize| match i {
            i if i == from => to,
            i if from < i && i <= to => i - 1,
            i if to <= i && i < from => i + 1,
            i => i,
        };
        self.current = shift(current);
        if let Some((drag_tab, _)) = self.dragging.as_mut() {
            *drag_tab = shift(*drag_tab);
        }
        true
    }

    /// The box the split at `(tab, path)` divides, for the frame to measure: it
    /// hands the `Rc` to an element that writes the box's size into it while the
    /// frame is painted. `None` when there is no split there.
    pub fn container(&self, tab: usize, path: &[bool]) -> Option<Rc<Cell<f32>>> {
        match self.tabs.get(tab).map(|tab| node_ref(&tab.root, path)) {
            Some(Some(Node::Split { container, .. })) => Some(container.clone()),
            _ => None,
        }
    }

    /// Take hold of the divider at `(tab, path)`, which is the node that divides.
    pub fn begin_drag(&mut self, tab: usize, path: &[bool], pointer: f32) -> bool {
        let Some(entry) = self.tabs.get_mut(tab) else {
            return false;
        };
        let Some(node) = node_mut(&mut entry.root, path) else {
            return false;
        };
        let Node::Split {
            size, container, ..
        } = node
        else {
            return false;
        };
        // What is on screen, not what is remembered.
        let laid_out = container.get();
        let ceiling = if laid_out > 0. {
            (laid_out - DIVIDER - MIN_PANE).max(MIN_PANE)
        } else {
            // Not measured yet, which is the first frame: the split's own
            // bounds are all a drag started then can have.
            MAX_PANE
        };
        size.set_bounds(MIN_PANE, ceiling);
        size.begin_from(pointer, size.width().min(ceiling));
        self.dragging = Some((tab, path.to_vec()));
        true
    }

    /// Follow the pointer, on the divider the drag started from. Always the `1.`
    /// sign: the first child is the one on the left (or on top), so its trailing
    /// edge is the one that follows.
    pub fn drag_to(&mut self, pointer: f32) -> bool {
        let Some((tab, path)) = self.dragging.clone() else {
            return false;
        };
        let Some(Some(Node::Split { size, .. })) = self
            .tabs
            .get_mut(tab)
            .map(|tab| node_mut(&mut tab.root, &path))
        else {
            return false;
        };
        size.move_to(pointer, 1.)
    }

    pub fn end_drag(&mut self) {
        let Some((tab, path)) = self.dragging.take() else {
            return;
        };
        if let Some(Some(Node::Split { size, .. })) = self
            .tabs
            .get_mut(tab)
            .map(|tab| node_mut(&mut tab.root, &path))
        {
            size.end();
        }
    }

    /// The divider being dragged, so the frame can say so: the seam goes to the
    /// accent colour while it is moving.
    pub fn dragging(&self) -> Option<&(usize, Path)> {
        self.dragging.as_ref()
    }

    /// Which way the divider the drag is on divides, so the pointer is read along the axis the
    /// divider moves on: a `x` drag on a horizontal divider is a pointer moving sideways, which
    /// is no evidence about where the seam is.
    pub fn dragging_axis(&self) -> Option<Axis> {
        let (tab, path) = self.dragging.as_ref()?;
        match self.tabs.get(*tab).map(|tab| node_ref(&tab.root, path)) {
            Some(Some(Node::Split { axis, .. })) => Some(*axis),
            _ => None,
        }
    }
}

/// `split::Right` — side by side, dividing the pane with the keyboard (`cmd-d`).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = split, no_json)]
pub struct Right;

/// `split::Down` — one above the other (`cmd-shift-d`).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = split, no_json)]
pub struct Down;

/// Close one pane: what the `×` in a pane's corner dispatches, because a click on a button
/// knows which pane it is in and the keyboard only knows which pane has the focus.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = split, no_json)]
pub struct ClosePaneAt {
    pub pane: Pane,
}

/// `split::ClosePane` — `cmd-w`: the pane with the keyboard and its terminal go
/// away, and the rest of the tree takes the area. With one pane that is the tab
/// closing, which is the same intent either way.
#[derive(Clone, PartialEq, Debug, Default, Action)]
#[action(namespace = split, no_json)]
pub struct ClosePane;

/// The pane at `path`, if there is a pane there.
fn leaf_pane(node: &Node, path: &[bool]) -> Option<Pane> {
    match (path.split_first(), node) {
        (None, Node::Pane { pane }) => Some(*pane),
        (Some((false, rest)), Node::Split { first, .. }) => leaf_pane(first, rest),
        (Some((true, rest)), Node::Split { second, .. }) => leaf_pane(second, rest),
        _ => None,
    }
}

/// The node at `path`: a pane, or the split whose divider sits there.
fn node_ref<'a>(node: &'a Node, path: &[bool]) -> Option<&'a Node> {
    match path.split_first() {
        None => Some(node),
        Some((false, rest)) => match node {
            Node::Split { first, .. } => node_ref(first, rest),
            Node::Pane { .. } => None,
        },
        Some((true, rest)) => match node {
            Node::Split { second, .. } => node_ref(second, rest),
            Node::Pane { .. } => None,
        },
    }
}

/// The node at `path`: a pane, or the split whose divider sits there.
fn node_mut<'a>(node: &'a mut Node, path: &[bool]) -> Option<&'a mut Node> {
    match path.split_first() {
        None => Some(node),
        Some((false, rest)) => match node {
            Node::Split { first, .. } => node_mut(first, rest),
            Node::Pane { .. } => None,
        },
        Some((true, rest)) => match node {
            Node::Split { second, .. } => node_mut(second, rest),
            Node::Pane { .. } => None,
        },
    }
}

fn collect_panes(node: &Node, out: &mut Vec<Pane>) {
    match node {
        Node::Pane { pane } => out.push(*pane),
        Node::Split { first, second, .. } => {
            collect_panes(first, out);
            collect_panes(second, out);
        }
    }
}

/// Fill `path` with the way to the pane holding `pane`.
fn path_of_pane(node: &Node, pane: Pane, path: &mut Path) -> bool {
    match node {
        Node::Pane { pane: here } => *here == pane,
        Node::Split { first, second, .. } => {
            path.push(false);
            if path_of_pane(first, pane, path) {
                return true;
            }
            path.pop();
            path.push(true);
            if path_of_pane(second, pane, path) {
                return true;
            }
            path.pop();
            false
        }
    }
}

/// Fill `path` with the way to the first pane in draw order.
fn first_path(node: &Node, path: &mut Path) {
    if let Node::Split { first, .. } = node {
        path.push(false);
        first_path(first, path);
    }
}

/// Replace the split at `node` with the sibling of the child on `side`: the
/// whole tree collapsing in on the pane that left. `false` when there is no
/// split there.
fn collapse(node: &mut Node, side: bool) -> bool {
    if !matches!(node, Node::Split { .. }) {
        return false;
    }
    let kept = match node {
        Node::Split { first, second, .. } => {
            // `side` is the side the pane that left was on; the sibling is the
            // other one, and it is what takes the area.
            let (keep, _gone) = if side {
                (first, second)
            } else {
                (second, first)
            };
            std::mem::replace(&mut **keep, Node::Pane { pane: 0 })
        }
        Node::Pane { .. } => return false,
    };
    *node = kept;
    true
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: this file globs gpui in, and gpui's own `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one
    // and every `#[test]` below expands into itself.
    use super::{Axis, DIVIDER, Layout, MAX_PANE, MIN_PANE, Node, Pane};

    /// A layout with one tab holding `pane`.
    fn with_tab(pane: Pane) -> Layout {
        let mut layout = Layout::default();
        layout.add_tab(pane);
        layout
    }

    /// A layout with one tab, split once along `axis` with `second` in the new
    /// half.
    fn split_once(axis: Axis, second: Pane) -> Layout {
        let mut layout = with_tab(0);
        assert!(layout.split(0, &[], axis, second));
        layout
    }

    #[test]
    fn a_moved_tab_keeps_the_one_on_screen() {
        let mut layout = with_tab(0);
        layout.add_tab(1);
        layout.add_tab(2);
        layout.set_current(1);
        assert!(layout.move_tab(0, 2));
        assert_eq!(layout.panes(), vec![1, 2, 0]);
        assert_eq!(layout.focused_pane(), Some(1));
        assert!(layout.move_tab(2, 0));
        assert_eq!(layout.panes(), vec![0, 1, 2]);
        assert_eq!(layout.focused_pane(), Some(1));
        assert!(layout.move_tab(1, 0));
        assert_eq!(layout.panes(), vec![1, 0, 2]);
        assert_eq!(layout.focused_pane(), Some(1));
        assert!(!layout.move_tab(0, 3));
    }

    #[test]
    fn a_worktree_with_nothing_open_has_no_tabs() {
        let layout = Layout::default();
        assert_eq!(layout.tab_count(), 0);
        assert_eq!(layout.focused_pane(), None);
    }

    /// The whole point of this shape: a split is content under its tab, so the
    /// bar still has one row.
    #[test]
    fn a_split_adds_a_pane_and_not_a_tab() {
        let layout = split_once(Axis::SideBySide, 1);
        assert_eq!(layout.tab_count(), 1, "the split became a tab of its own");
        assert_eq!(layout.panes_of(0), vec![0, 1]);
        assert_eq!(
            layout.focused_pane(),
            Some(1),
            "the new pane takes the keyboard"
        );
        assert_eq!(layout.focused_path(), vec![true]);
        // And the row says what it is: one tab, divided this way.
        assert_eq!(layout.tab_axis(0), Some(Axis::SideBySide));
    }

    #[test]
    fn the_first_pane_can_be_the_one_with_the_keyboard() {
        let mut layout = split_once(Axis::SideBySide, 1);
        layout.split(0, &[false], Axis::Stacked, 2);
        assert_eq!(layout.focused_pane(), Some(2));
        layout.focus_first(0);
        assert_eq!(layout.focused_path(), vec![false, false]);
        assert_eq!(layout.focused_pane(), Some(0));
    }

    #[test]
    fn a_pane_inside_a_split_can_be_split_again() {
        let mut layout = split_once(Axis::SideBySide, 1);
        assert!(layout.split(0, &[false], Axis::Stacked, 2));
        assert_eq!(layout.tab_count(), 1);
        assert_eq!(layout.panes_of(0), vec![0, 2, 1]);
        assert_eq!(layout.focused_path(), vec![false, true]);
        // The row's mark is the outermost split, not this one.
        assert_eq!(layout.tab_axis(0), Some(Axis::SideBySide));
        // A divider is not a pane: nothing to divide.
        assert!(
            !layout.split(0, &[false], Axis::Stacked, 3),
            "a divider is not a pane: nothing to divide"
        );
    }

    #[test]
    fn two_tabs_have_two_trees_and_the_keyboard_is_where_it_was_left() {
        let mut layout = with_tab(0);
        layout.split(0, &[], Axis::Stacked, 1);
        layout.add_tab(2);
        assert_eq!(layout.tab_count(), 2);
        assert_eq!(layout.current(), 1);
        assert_eq!(layout.panes_of(1), vec![2], "the new tab is one pane");

        // Back to the split tab: the pane that had the keyboard there has it
        // again, which is what makes clicking between tabs not lose your place.
        assert!(layout.set_current(0));
        assert_eq!(layout.focused_pane(), Some(1));
        assert_eq!(layout.tab_focused_pane(1), Some(2));
    }

    #[test]
    fn a_pane_knows_its_tab_and_its_path() {
        let mut layout = with_tab(0);
        layout.split(0, &[], Axis::SideBySide, 1);
        layout.add_tab(2);
        layout.split(1, &[], Axis::Stacked, 3);
        assert_eq!(layout.path_of_pane(3), Some((1, vec![true])));
        assert_eq!(layout.path_of_pane(0), Some((0, vec![false])));
        assert_eq!(layout.path_of_pane(9), None, "a pane no tab holds");

        // Focusing by content names the tab too, which is what a click in a
        // pane does without knowing where it is.
        assert!(layout.focus_pane(0));
        assert_eq!(layout.current(), 0);
        assert_eq!(layout.focused_pane(), Some(0));
    }

    #[test]
    fn closing_a_pane_gives_the_area_to_its_sibling() {
        let mut layout = split_once(Axis::SideBySide, 1);
        // The keyboard was in the second half; closing the first one keeps it
        // there.
        let (gone, tab) = layout.close_pane(0, &[false]).expect("the pane was there");
        assert_eq!(gone, 0);
        assert_eq!(tab, Some(0));
        assert_eq!(layout.panes_of(0), vec![1]);
        assert_eq!(layout.tab_count(), 1);
        assert_eq!(layout.focused_pane(), Some(1));

        // And the other way round: the half that leaves is the focused one, so
        // the keyboard lands on the sibling.
        let mut layout = split_once(Axis::SideBySide, 1);
        let (gone, _) = layout.close_pane(0, &[true]).expect("the pane was there");
        assert_eq!(gone, 1);
        assert_eq!(layout.panes_of(0), vec![0]);
        assert_eq!(layout.focused_pane(), Some(0));
        assert_eq!(layout.tab_axis(0), None, "one pane is not a split");
    }

    /// A pane inside a split inside a split: closing it has to leave the keyboard
    /// on a pane that exists, one level up.
    #[test]
    fn closing_a_pane_inside_a_split_leaves_the_keyboard_somewhere_that_exists() {
        let mut layout = split_once(Axis::SideBySide, 1);
        layout.split(0, &[true], Axis::Stacked, 2);
        assert_eq!(layout.focused_pane(), Some(2));
        let (gone, _) = layout
            .close_pane(0, &[true, true])
            .expect("the pane was there");
        assert_eq!(gone, 2);
        assert_eq!(layout.panes_of(0), vec![0, 1]);
        assert_eq!(layout.focused_pane(), Some(1));
        // And a path that is a split and not a pane closes nothing: there is no
        // pane there to leave.
        assert!(layout.close_pane(0, &[false]).is_some());
    }

    #[test]
    fn closing_the_last_pane_takes_its_tab() {
        let mut layout = with_tab(0);
        layout.add_tab(1);
        let (gone, tab) = layout.close_pane(1, &[]).expect("the pane was there");
        assert_eq!(gone, 1);
        assert_eq!(tab, None, "the tab went with its last pane");
        assert_eq!(layout.tab_count(), 1);
        assert_eq!(layout.current(), 0, "the tab on screen is one that exists");
    }

    #[test]
    fn closing_a_tab_hands_back_every_pane_it_held() {
        let mut layout = with_tab(0);
        layout.split(0, &[], Axis::SideBySide, 1);
        layout.split(0, &[true], Axis::Stacked, 2);
        layout.add_tab(3);

        let mut gone = layout.remove_tab(0);
        gone.sort();
        assert_eq!(gone, vec![0, 1, 2], "the whole tree came back");
        assert_eq!(layout.tab_count(), 1);
        assert_eq!(layout.panes_of(0), vec![3]);
        assert_eq!(layout.current(), 0);
        assert!(layout.remove_tab(5).is_empty(), "no such tab");
    }

    #[test]
    fn a_drag_moves_one_divider_and_stops_at_its_bounds() {
        let mut layout = split_once(Axis::SideBySide, 1);
        assert!(layout.dragging().is_none());
        // A pane is not a divider, and neither is a tab with one pane.
        assert!(!layout.begin_drag(0, &[false], 100.));
        assert!(layout.begin_drag(0, &[], 100.));
        assert_eq!(
            layout.dragging().map(|(tab, path)| (*tab, path.clone())),
            Some((0, vec![]))
        );
        assert_eq!(layout.dragging_axis(), Some(Axis::SideBySide));

        // The box has not been measured, so the bounds are the drag's own: a drag to the left shrinks
        // the first pane to its minimum, and one past the ceiling stops there.
        layout.drag_to(-9_000.);
        assert_eq!(split_width(&layout), MIN_PANE);
        assert!(!layout.drag_to(-9_100.), "already at the floor");
        layout.drag_to(9_000.);
        assert_eq!(split_width(&layout), MAX_PANE);
        // Both panes are still there: a drag moves the divider, not the tree.
        assert_eq!(layout.panes_of(0), vec![0, 1]);

        layout.end_drag();
        assert!(layout.dragging().is_none());
        assert!(!layout.drag_to(400.), "a move with no drag in flight");
    }

    /// The width the split of a one-split layout holds. What a drag is asserted
    /// about, since the drawn size is the frame's business.
    fn split_width(layout: &Layout) -> f32 {
        match layout.root(0) {
            Some(Node::Split { size, .. }) => size.width(),
            _ => panic!("no split"),
        }
    }

    /// The ceiling a drag may move within comes from the box the split was last
    /// drawn in — not from the size it remembers, which is a window's width and
    /// may be no relation to what is on screen now.
    #[test]
    fn the_divider_bounds_within_the_box_it_is_in() {
        let mut layout = split_once(Axis::Stacked, 1);
        layout.container(0, &[]).expect("a split here").set(600.);
        assert!(layout.begin_drag(0, &[], 300.));
        layout.drag_to(9_000.);
        assert_eq!(split_width(&layout), 600. - DIVIDER - MIN_PANE);
        assert!(!layout.drag_to(9_100.), "already at the ceiling");
        layout.drag_to(0.);
        assert_eq!(split_width(&layout), MIN_PANE);
    }
}
