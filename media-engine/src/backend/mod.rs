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
    pub(crate) const fn new() -> Self {
        Self {
            night: false,
            target: NightModeTarget::Auto,
        }
    }

    /// `(night, chosen manually)`, the pair the daemon is told.
    pub(crate) const fn reported(&self) -> (bool, bool) {
        (self.night, !matches!(self.target, NightModeTarget::Auto))
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
