//! Presentation timestamps for records that carry no capture time.
//!
//! The reference engine keeps one monotonic counter for control records (night
//! mode, speaker commands) and advances it by a fixed step, while captured
//! frames use the V4L2 buffer timestamp. The counter is owned by the caller so
//! each thread or pipeline stage can keep its own sequence.

/// Timestamp step of the reference engine, in microseconds (50 ms).
pub const DEFAULT_STEP_US: u64 = 50_000;

/// A monotonic microsecond timestamp source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counter {
    next_us: u64,
    step_us: u64,
}

impl Counter {
    /// Create a counter that advances by `step_us` on every call.
    pub const fn new(step_us: u64) -> Self {
        Self {
            next_us: 0,
            step_us,
        }
    }

    /// Return the next timestamp, advancing the counter.
    ///
    /// Wrapping matches the reference engine's unsigned counter arithmetic.
    pub fn advance(&mut self) -> u64 {
        self.next_us = self.next_us.wrapping_add(self.step_us);
        self.next_us
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new(DEFAULT_STEP_US)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_by_the_reference_step() {
        let mut counter = Counter::default();

        assert_eq!(counter.advance(), 50_000);
        assert_eq!(counter.advance(), 100_000);
        assert_eq!(counter.advance(), 150_000);
    }

    #[test]
    fn honours_a_custom_step() {
        let mut counter = Counter::new(20_000);

        assert_eq!(counter.advance(), 20_000);
        assert_eq!(counter.advance(), 40_000);
    }

    #[test]
    fn wraps_instead_of_panicking() {
        let mut counter = Counter::new(u64::MAX);

        assert_eq!(counter.advance(), u64::MAX);
        assert_eq!(counter.advance(), u64::MAX - 1);
    }
}
