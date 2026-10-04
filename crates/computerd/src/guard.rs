//! Stops an agent that repeats the same batch while the screen does not change.

use computer_protocol::Action;

/// Identical batches that may leave the screen unchanged in a row. The next one is refused.
const MAX_UNCHANGED_REPEATS: u8 = 3;

/// What a finished batch did to the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Changed,
    Unchanged,
    /// The batch ended without a screenshot, so the effect is not known.
    Unknown,
}

/// Counts identical batches that left the frame unchanged.
#[derive(Debug, Default)]
pub(crate) struct LoopGuard {
    last: Vec<Action>,
    unchanged: u8,
    /// Id of the frame the last counted batch left unchanged.
    frame: Option<u64>,
}

/// Batches of scroll, pointer, and key actions, with waits. Typing or focusing
/// changes what the next batch does, so such a batch is never counted.
fn is_countable(batch: &[Action]) -> bool {
    let only_inputs_and_waits = batch
        .iter()
        .all(|action| !matches!(action, Action::Type { .. } | Action::Focus { .. }));
    only_inputs_and_waits
        && batch
            .iter()
            .any(|action| !matches!(action, Action::Wait { .. }))
}

impl LoopGuard {
    /// Forgets the count when the screen changed on its own since the last batch.
    pub(crate) fn sync(&mut self, frame: u64) {
        if self.frame != Some(frame) {
            self.reset();
        }
    }

    /// Whether `batch` is the one repeated [`MAX_UNCHANGED_REPEATS`] times already.
    /// Returns how many times it ran when it must be refused.
    pub(crate) fn refusal(&self, batch: &[Action]) -> Option<u8> {
        (is_countable(batch) && self.unchanged >= MAX_UNCHANGED_REPEATS && self.last == batch)
            .then_some(self.unchanged)
    }

    /// Records a batch that ran.
    pub(crate) fn record(&mut self, batch: &[Action], outcome: Outcome, frame: u64) {
        self.frame = Some(frame);
        if outcome != Outcome::Unchanged || !is_countable(batch) {
            self.reset();
        } else if self.last == batch {
            self.unchanged = self.unchanged.saturating_add(1);
        } else {
            batch.clone_into(&mut self.last);
            self.unchanged = 1;
        }
    }

    /// Forgets the count, for example after a failed batch.
    pub(crate) fn reset(&mut self) {
        self.last.clear();
        self.unchanged = 0;
        self.frame = None;
    }
}

#[cfg(test)]
mod tests {
    use computer_protocol::{Button, Direction, Point};

    use super::*;

    fn scroll() -> Vec<Action> {
        vec![Action::Scroll {
            at: None,
            direction: Direction::Down,
            amount: 3,
        }]
    }

    fn click() -> Vec<Action> {
        vec![Action::Click {
            at: Point { x: 5, y: 5 },
            button: Button::Left,
        }]
    }

    fn typing() -> Vec<Action> {
        vec![Action::Type {
            text: "x".to_owned(),
        }]
    }

    fn run(guard: &mut LoopGuard, batch: &[Action], outcome: Outcome) -> bool {
        guard.sync(1);
        let refused = guard.refusal(batch).is_some();
        if !refused {
            guard.record(batch, outcome, 1);
        }
        refused
    }

    #[test]
    fn the_fourth_identical_unchanged_batch_is_refused() {
        let mut guard = LoopGuard::default();
        for _ in 0..3 {
            assert!(!run(&mut guard, &scroll(), Outcome::Unchanged));
        }
        assert_eq!(guard.refusal(&scroll()), Some(3));
        assert!(guard.refusal(&click()).is_none());
    }

    #[test]
    fn a_different_batch_a_change_or_typing_restarts_the_count() {
        for interruption in [
            (click(), Outcome::Unchanged),
            (scroll(), Outcome::Changed),
            (typing(), Outcome::Unchanged),
            (scroll(), Outcome::Unknown),
        ] {
            let mut guard = LoopGuard::default();
            for _ in 0..2 {
                assert!(!run(&mut guard, &scroll(), Outcome::Unchanged));
            }
            assert!(!run(&mut guard, &interruption.0, interruption.1));
            for _ in 0..3 {
                assert!(
                    !run(&mut guard, &scroll(), Outcome::Unchanged),
                    "{interruption:?} should restart the count"
                );
            }
            assert!(run(&mut guard, &scroll(), Outcome::Unchanged));
        }
    }

    #[test]
    fn a_screen_that_changed_on_its_own_between_batches_restarts_the_count() {
        let mut guard = LoopGuard::default();
        for _ in 0..3 {
            assert!(!run(&mut guard, &scroll(), Outcome::Unchanged));
        }
        guard.sync(2);
        assert!(guard.refusal(&scroll()).is_none());
    }

    #[test]
    fn typing_batches_are_never_refused_and_waits_alone_do_not_count() {
        let mut guard = LoopGuard::default();
        for _ in 0..6 {
            assert!(!run(&mut guard, &typing(), Outcome::Unchanged));
        }
        let wait = vec![Action::Wait { ms: 100 }];
        for _ in 0..6 {
            assert!(!run(&mut guard, &wait, Outcome::Unchanged));
        }
    }

    #[test]
    fn waits_inside_a_scroll_batch_do_not_hide_the_repeat() {
        let mut batch = scroll();
        batch.push(Action::Wait { ms: 100 });
        let mut guard = LoopGuard::default();
        for _ in 0..3 {
            assert!(!run(&mut guard, &batch, Outcome::Unchanged));
        }
        assert!(run(&mut guard, &batch, Outcome::Unchanged));
    }
}
