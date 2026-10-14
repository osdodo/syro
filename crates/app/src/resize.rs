//! Dragging a panel's edge to resize it.

use gpui_kit::*;

/// A live drag of a panel's edge.
pub struct WidthDrag {
    /// `(pointer x, width)` when the drag began, in window coordinates. `None`
    /// between drags.
    start: Option<(f32, f32)>,
    width: f32,
    min: f32,
    max: f32,
}

impl WidthDrag {
    pub fn new(width: f32, min: f32, max: f32) -> Self {
        Self {
            start: None,
            width: width.clamp(min, max),
            min,
            max,
        }
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn dragging(&self) -> bool {
        self.start.is_some()
    }

    /// Take hold of the edge at the pointer's x. A drag already in flight is
    /// left alone: two mouse-downs cannot both own the edge.
    pub fn begin(&mut self, pointer_x: f32) {
        if self.start.is_none() {
            self.start = Some((pointer_x, self.width));
        }
    }

    /// Take hold of the edge at the pointer's x, from a width that is not the one remembered.
    pub fn begin_from(&mut self, pointer_x: f32, width: f32) {
        if self.start.is_none() {
            self.width = width.clamp(self.min, self.max);
            self.start = Some((pointer_x, self.width));
        }
    }

    /// Narrow the bounds a drag is clamped to.
    pub fn set_bounds(&mut self, min: f32, max: f32) {
        self.min = min.max(0.);
        self.max = max.max(self.min);
        self.width = self.width.clamp(self.min, self.max);
    }

    /// Follow the pointer.
    pub fn move_to(&mut self, pointer_x: f32, sign: f32) -> bool {
        let Some((start_x, start_width)) = self.start else {
            return false;
        };
        let width = (start_width + (pointer_x - start_x) * sign).clamp(self.min, self.max);
        let moved = width != self.width;
        self.width = width;
        moved
    }

    pub fn end(&mut self) {
        self.start = None;
    }
}

/// Register this frame's window-level listeners for a drag: every move while the left button is
/// held, and the release.
pub fn follow_pointer<T: 'static>(
    window: &mut Window,
    view: &WeakEntity<T>,
    apply: impl Fn(&mut T, Option<Point<Pixels>>, &mut Context<T>) + Clone + 'static,
) {
    let moving_view = view.clone();
    let moving = apply.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
        // The capture phase is the platform's own pass over the event, before
        // any element sees it: it fires wherever the pointer is, which is the
        // whole point.
        if phase != DispatchPhase::Capture || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        moving_view
            .update(cx, |view, cx| moving(view, Some(event.position), cx))
            .ok();
    });

    let release_view = view.clone();
    window.on_mouse_event(move |_event: &MouseUpEvent, phase, _window, cx| {
        if phase != DispatchPhase::Capture {
            return;
        }
        release_view
            .update(cx, |view, cx| apply(view, None, cx))
            .ok();
    });
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one.
    use super::WidthDrag;

    #[test]
    fn a_left_panel_grows_with_the_pointer_and_stops_at_its_bounds() {
        let mut drag = WidthDrag::new(240., 220., 500.);
        assert_eq!(drag.width(), 240.);
        assert!(!drag.dragging());

        drag.begin(100.);
        assert!(drag.dragging());
        assert!(drag.move_to(140., 1.));
        assert_eq!(drag.width(), 280.);
        // Past the maximum the width stops, and a move that changes nothing
        // says so: the caller does not redraw for it.
        drag.move_to(900., 1.);
        assert_eq!(drag.width(), 500.);
        assert!(!drag.move_to(950., 1.));
        drag.move_to(-900., 1.);
        assert_eq!(drag.width(), 220.);

        drag.end();
        assert!(!drag.dragging());
        assert!(!drag.move_to(400., 1.), "a move with no drag in flight");
    }

    /// A width that comes from somewhere other than this drag — a remembered
    /// one, from a window that was wider — is clamped on the way in, or a panel
    /// opens off the edge of a narrow window.
    #[test]
    fn a_width_that_arrives_out_of_range_is_clamped() {
        assert_eq!(WidthDrag::new(9_000., 220., 500.).width(), 500.);
        assert_eq!(WidthDrag::new(1., 220., 500.).width(), 220.);
        // In range is kept exactly: clamping is not rounding to the default.
        assert_eq!(WidthDrag::new(280., 220., 500.).width(), 280.);
    }

    #[test]
    fn a_right_panel_grows_the_other_way() {
        let mut drag = WidthDrag::new(260., 160., 500.);
        drag.begin(1000.);
        assert!(drag.move_to(960., -1.));
        assert_eq!(drag.width(), 300.);
        drag.move_to(900., -1.);
        assert_eq!(drag.width(), 360.);
        drag.end();
    }

    #[test]
    fn a_second_grab_does_not_restart_the_drag() {
        let mut drag = WidthDrag::new(240., 220., 500.);
        drag.begin(100.);
        drag.move_to(140., 1.);
        assert_eq!(drag.width(), 280.);
        // The grab that is already in flight owns the edge: this one is ignored,
        // so the width is still measured from 100 and 240.
        drag.begin(400.);
        drag.move_to(150., 1.);
        assert_eq!(
            drag.width(),
            290.,
            "a second grab re-based the drag instead of being ignored"
        );
    }
}
