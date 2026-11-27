//! A pointer move macOS reports in the wrong place, dropped before egui
//! sees it.
//!
//! In native full screen on a display other than the main one, a press
//! near the top of the window is followed at once by one mouse event whose
//! position is off by that display's place in the arrangement: on a 34"
//! display above a MacBook, every press on Connect at (66, 26) came with a
//! move to (1778, 1143), 1712 and 1117 points off. egui took it as the
//! pointer leaving while pressed, so the press became a drag over whatever
//! was there (the result table, which selected its rows) and the button
//! never clicked. On the main display the offset is nothing, so it never
//! shows there.
//!
//! The press and the release carry the right position (winit sends a move
//! to each first), so the first move after a press is dropped when it lands
//! far from it: a hand can't move that far between the press and the next
//! event, and the moves after it (and the release's own) put the pointer
//! where it is.

use egui::{Event, Pos2};

/// Farther than this from the press, the first move after it is the stray.
/// A display's place in the arrangement is hundreds of points at least.
const FAR: f32 = 200.0;

#[derive(Default)]
pub struct StrayMoves {
    /// A press whose first move after it has not come yet.
    pressed_at: Option<Pos2>,
}

impl StrayMoves {
    /// Drops the stray move from a frame's events (`App::raw_input_hook`).
    pub fn filter(&mut self, events: &mut Vec<Event>) {
        events.retain(|e| match e {
            Event::PointerButton { pos, pressed: true, .. } => {
                self.pressed_at = Some(*pos);
                true
            }
            Event::PointerButton { pressed: false, .. } | Event::PointerGone => {
                self.pressed_at = None;
                true
            }
            Event::PointerMoved(to) => self.pressed_at.take().is_none_or(|at| at.distance(*to) <= FAR),
            _ => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, PointerButton, pos2};

    fn press(x: f32, y: f32) -> Event {
        Event::PointerButton { pos: pos2(x, y), button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }
    }

    fn release(x: f32, y: f32) -> Event {
        Event::PointerButton { pos: pos2(x, y), button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }
    }

    fn moved(x: f32, y: f32) -> Event {
        Event::PointerMoved(pos2(x, y))
    }

    fn filtered(strays: &mut StrayMoves, mut events: Vec<Event>) -> Vec<Event> {
        strays.filter(&mut events);
        events
    }

    /// The frames logged on the 34" display: the move after each press is
    /// dropped, and the press and release stay where they were.
    #[test]
    fn the_far_move_after_a_press_is_dropped() {
        let mut s = StrayMoves::default();
        assert_eq!(filtered(&mut s, vec![moved(66.0, 26.0), press(66.0, 26.0), moved(1778.0, 1143.0)]), [moved(66.0, 26.0), press(66.0, 26.0)]);
        assert_eq!(filtered(&mut s, vec![moved(66.0, 26.0), release(66.0, 26.0)]), [moved(66.0, 26.0), release(66.0, 26.0)]);
        assert_eq!(filtered(&mut s, vec![moved(66.0, 18.0), press(66.0, 18.0), moved(1778.0, 1135.0)]), [moved(66.0, 18.0), press(66.0, 18.0)]);
    }

    /// The stray can come in the next frame's events.
    #[test]
    fn the_stray_in_the_next_frame_is_dropped_too() {
        let mut s = StrayMoves::default();
        assert_eq!(filtered(&mut s, vec![moved(77.0, 23.0), press(77.0, 23.0)]).len(), 2);
        assert_eq!(filtered(&mut s, vec![moved(1789.0, 1140.0)]), []);
    }

    /// A drag keeps every move: the first one is near the press, and
    /// only the first is looked at, so a fast drag later isn't cut.
    #[test]
    fn a_drag_keeps_its_moves() {
        let mut s = StrayMoves::default();
        let drag = vec![press(100.0, 400.0), moved(104.0, 402.0), moved(400.0, 420.0), moved(900.0, 430.0), release(900.0, 430.0)];
        assert_eq!(filtered(&mut s, drag.clone()), drag);
    }

    /// Without a press nothing is dropped, however far the pointer goes
    /// (it comes back into the window anywhere), and a release ends the
    /// watch for the stray.
    #[test]
    fn moves_without_a_press_are_kept() {
        let mut s = StrayMoves::default();
        let moves = vec![moved(10.0, 10.0), moved(1500.0, 900.0), Event::PointerGone, moved(3000.0, 20.0)];
        assert_eq!(filtered(&mut s, moves.clone()), moves);
        let click = vec![press(50.0, 50.0), release(50.0, 50.0), moved(2000.0, 1000.0)];
        assert_eq!(filtered(&mut s, click.clone()), click);
    }
}
