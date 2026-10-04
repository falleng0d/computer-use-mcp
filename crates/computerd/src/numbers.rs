//! Screen numbers. A screen's number is also its X display number and the end of its ports.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

pub(crate) const FIRST_SCREEN: u8 = 1;
pub(crate) const LAST_SCREEN: u8 = 16;

/// Lowest screen number from [`FIRST_SCREEN`] to [`LAST_SCREEN`] that is not in `used`.
fn lowest_free(used: &BTreeSet<u8>) -> Option<u8> {
    (FIRST_SCREEN..=LAST_SCREEN).find(|number| !used.contains(number))
}

/// The screen numbers in use. A [`Lease`] frees its number when dropped.
#[derive(Debug, Clone, Default)]
pub(crate) struct Numbers(Arc<Mutex<BTreeSet<u8>>>);

#[derive(Debug)]
pub(crate) struct Lease {
    numbers: Numbers,
    number: u8,
}

impl Numbers {
    /// Takes the lowest free number, or `None` when all 16 screens exist.
    pub(crate) fn lease(&self) -> Option<Lease> {
        let mut used = self
            .0
            .lock()
            .expect("the screen number lock is only held for short set updates");
        let number = lowest_free(&used)?;
        used.insert(number);
        Some(Lease {
            numbers: self.clone(),
            number,
        })
    }
}

impl Lease {
    pub(crate) fn number(&self) -> u8 {
        self.number
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.numbers
            .0
            .lock()
            .expect("the screen number lock is only held for short set updates")
            .remove(&self.number);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_reused_lowest_first_and_run_out_after_sixteen() {
        let numbers = Numbers::default();
        let mut leases: Vec<Lease> = (FIRST_SCREEN..=LAST_SCREEN)
            .map(|_| numbers.lease().unwrap())
            .collect();
        assert_eq!(leases[0].number(), 1);
        assert_eq!(leases[15].number(), 16);
        assert!(numbers.lease().is_none());

        drop(leases.remove(4));
        drop(leases.remove(1));
        assert_eq!(numbers.lease().unwrap().number(), 2);
    }
}
