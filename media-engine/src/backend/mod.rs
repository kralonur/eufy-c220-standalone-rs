//! What the engine does with daemon commands.
//!
//! The trait exists so the daemon protocol can be exercised without a camera:
//! [`MockBackend`] keeps the state the protocol exposes and logs every command,
//! while [`HardwareBackend`] - which only exists on the camera target - drives the
//! audio devices and keeps the microphone's record producer running. Both appear on the wire in exactly the same way.

use crate::client::RecordSink;
use crate::commands::{Command, NightModeTarget};
use crate::platform::Error;
use crate::platform::signal::Shutdown;
use alloc::sync::Arc;

#[cfg(all(target_arch = "arm", feature = "hardware"))]
mod hardware;
#[cfg(all(target_arch = "arm", feature = "hardware"))]
mod image;
mod mock;
#[cfg(all(target_arch = "arm", feature = "hardware"))]
mod video;

#[cfg(all(target_arch = "arm", feature = "hardware"))]
pub use hardware::HardwareBackend;
pub use mock::MockBackend;
#[cfg(all(target_arch = "arm", feature = "hardware"))]
pub use video::{VideoConfig, VideoPipeline};

/// The device side of the engine.
pub trait Backend {
    /// Open the devices this backend owns and start its producer threads.
    ///
    /// Called once before the engine starts accepting sessions. `sink` is how a
    /// producer reaches the daemon and `shutdown` is how its thread learns to
    /// stop; both are shared with the engine.
    ///
    /// A failure is reported and the engine keeps serving: the daemon should
    /// still be able to talk to the parts that did come up, and a device that is
    /// missing is visible in the log rather than in a socket that never answers.
    fn start(&mut self, sink: Arc<RecordSink>, shutdown: Arc<Shutdown>) -> Result<(), Error> {
        let _ = (sink, shutdown);
        Ok(())
    }

    /// Handle one command from the daemon.
    ///
    /// The sink is the connection the command arrived on, so a backend can report
    /// state changes (night mode) back to the daemon.
    fn command(&mut self, command: Command<'_>, sink: &RecordSink) -> Result<(), Error>;

    /// Current night state: `(night, chosen manually)`.
    ///
    /// It is sent to the daemon whenever a session starts.
    fn night_mode(&self) -> (bool, bool);

    /// Release everything the backend owns.
    fn shutdown(&mut self);
}

/// The night-mode state both backends keep.
///
/// The daemon can force night, force day, or hand the decision back to the
/// engine; what the image currently is belongs to the device path, which reports
/// it here once it knows. Only a change of the visible state is sent back to the
/// daemon.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NightState {
    night: bool,
    target: NightModeTarget,
}

impl NightState {
    /// A camera that starts in day mode, choosing for itself.
    pub const fn new() -> Self {
        Self {
            night: false,
            target: NightModeTarget::Auto,
        }
    }

    /// `(night, chosen manually)`, the pair the daemon is told.
    pub(crate) const fn reported(&self) -> (bool, bool) {
        (self.night, !matches!(self.target, NightModeTarget::Auto))
    }

    /// The mode the daemon commanded, or `None` while the engine decides.
    ///
    /// Only the camera's night monitor reads this: the mock backend has no
    /// hardware for a command to own.
    #[cfg(all(target_arch = "arm", feature = "hardware"))]
    pub(crate) const fn commanded(&self) -> Option<bool> {
        match self.target {
            NightModeTarget::Auto => None,
            NightModeTarget::Night => Some(true),
            NightModeTarget::Day => Some(false),
        }
    }

    /// Record what the daemon asked for.
    ///
    /// Returns `Some(night)` when the visible state changes, which is when the
    /// daemon is told about it; `Auto` leaves the state to the device path.
    pub(crate) fn apply(&mut self, target: NightModeTarget) -> Option<bool> {
        self.target = target;
        let wanted = match target {
            NightModeTarget::Auto => return None,
            NightModeTarget::Night => true,
            NightModeTarget::Day => false,
        };
        self.set(wanted)
    }

    /// Record what the device path decided, so the daemon hears about it.
    pub(crate) fn set(&mut self, night: bool) -> Option<bool> {
        if night == self.night {
            return None;
        }
        self.night = night;
        Some(night)
    }
}

/// Samples a new verdict has to hold before the engine acts on it.
const CONFIRM_SAMPLES: u32 = 5;

/// Samples the engine waits after a switch before asking again.
///
/// The statistics still describe the previous lighting for a while: the filter has
/// just moved, the scene changed and the AE is converging on it.
const SETTLE_SAMPLES: u32 = 10;

/// The detector's opinion of one sample, as the monitor sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The sample says nothing (no statistics, or a commanded mode).
    Unknown,
    /// The sample says day or night.
    Day,
    /// The sample says night.
    Night,
}

impl Verdict {
    /// Whether this verdict asks for night mode.
    pub const fn night(self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::Day => Some(false),
            Self::Night => Some(true),
        }
    }
}

/// What the hardware should do after a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing; the mode stays as it is.
    Hold,
    /// Switch to the given mode.
    Switch(bool),
}

/// The confirmation and settle windows around a mode change.
///
/// The decision itself is the library's; this is the engine's guard against a
/// single odd sample, and it is separate from the hardware so it can be tested
/// without a camera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Detector {
    confirm: u32,
    settle: u32,
}

impl Detector {
    /// A detector that has not seen anything yet.
    pub const fn new() -> Self {
        Self {
            confirm: 0,
            settle: 0,
        }
    }

    /// What to do with one sample.
    ///
    /// `commanded` is the daemon's target when it has one, and `current` is the
    /// mode the hardware is in.
    pub fn step(&mut self, commanded: Option<bool>, current: bool, verdict: Verdict) -> Step {
        if let Some(target) = commanded {
            // A command owns the hardware: the detector is not consulted, and the
            // settle window is armed so that a stale verdict cannot fight it.
            self.confirm = 0;
            if target == current {
                self.settle = SETTLE_SAMPLES;
                return Step::Hold;
            }
            self.settle = SETTLE_SAMPLES;
            return Step::Switch(target);
        }

        if self.settle > 0 {
            // The statistics still describe the previous lighting.
            self.settle -= 1;
            self.confirm = 0;
            return Step::Hold;
        }

        let Some(night) = verdict.night() else {
            return Step::Hold;
        };
        if night == current {
            self.confirm = 0;
            return Step::Hold;
        }

        self.confirm += 1;
        if self.confirm < CONFIRM_SAMPLES {
            return Step::Hold;
        }
        self.confirm = 0;
        self.settle = SETTLE_SAMPLES;
        Step::Switch(night)
    }
}

#[cfg(test)]
mod detector_tests {
    use super::{CONFIRM_SAMPLES, Detector, SETTLE_SAMPLES, Step, Verdict};

    #[test]
    fn a_commanded_mode_switches_at_once_and_owns_the_hardware() {
        let mut detector = Detector::new();
        // Night is asked for while the hardware is in day mode.
        assert_eq!(
            detector.step(Some(true), false, Verdict::Unknown),
            Step::Switch(true)
        );
        // Once there, the command holds it there.
        assert_eq!(
            detector.step(Some(true), true, Verdict::Unknown),
            Step::Hold
        );
        // A detector verdict cannot fight the command, and the settle window has
        // been armed so a stale one cannot either.
        assert_eq!(detector.step(Some(true), true, Verdict::Day), Step::Hold);
        assert_eq!(detector.step(Some(true), true, Verdict::Day), Step::Hold);
    }

    #[test]
    fn one_verdict_is_not_enough_and_opposite_ones_cancel() {
        let mut detector = Detector::new();
        // Four samples asking for night: still nothing.
        for _ in 0..CONFIRM_SAMPLES - 1 {
            assert_eq!(detector.step(None, false, Verdict::Night), Step::Hold);
        }
        // A single day sample resets the count, so the four are lost.
        assert_eq!(detector.step(None, false, Verdict::Day), Step::Hold);
        for _ in 0..CONFIRM_SAMPLES - 1 {
            assert_eq!(detector.step(None, false, Verdict::Night), Step::Hold);
        }
        // The fifth consecutive sample switches.
        assert_eq!(
            detector.step(None, false, Verdict::Night),
            Step::Switch(true)
        );
    }

    #[test]
    fn the_sensor_is_left_alone_while_it_settles() {
        let mut detector = Detector::new();
        for _ in 0..CONFIRM_SAMPLES {
            detector.step(None, false, Verdict::Night);
        }
        // The switch armed the settle window: ten samples are ignored, and they
        // are counted even when they disagree.
        for _ in 0..SETTLE_SAMPLES {
            assert_eq!(detector.step(None, true, Verdict::Day), Step::Hold);
        }
        // After that the detector is believed again, over the full window.
        for _ in 0..CONFIRM_SAMPLES - 1 {
            assert_eq!(detector.step(None, true, Verdict::Day), Step::Hold);
        }
        assert_eq!(detector.step(None, true, Verdict::Day), Step::Switch(false));
    }

    #[test]
    fn missing_statistics_change_nothing() {
        let mut detector = Detector::new();
        for _ in 0..100 {
            assert_eq!(detector.step(None, false, Verdict::Unknown), Step::Hold);
        }
        // A sample with no statistics is skipped rather than counted: four
        // verdicts then one timeout then one more verdict is five, and switches -
        // which is what the reference engine does too, because its loop simply
        // does nothing when the statistics call returns nothing.
        for _ in 0..CONFIRM_SAMPLES - 1 {
            assert_eq!(detector.step(None, false, Verdict::Night), Step::Hold);
        }
        assert_eq!(detector.step(None, false, Verdict::Unknown), Step::Hold);
        assert_eq!(
            detector.step(None, false, Verdict::Night),
            Step::Switch(true)
        );
    }

    #[test]
    fn a_mode_matching_the_hardware_needs_no_confirmation() {
        let mut detector = Detector::new();
        for _ in 0..10 {
            assert_eq!(detector.step(None, true, Verdict::Night), Step::Hold);
        }
    }
}
