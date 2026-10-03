//! Camera-local PTZ control through the preserved `/dev/motor` driver.
//!
//! The ioctl numbers, initialization array, axis limits, GPIO assignments, and
//! speed mapping are reverse-engineered from the C220's `motor_drv.ko` path.
//! This module does not use the vendor/P2P protocol.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracing::info;

/// Default PTZ character device exposed by the camera kernel module.
pub const DEFAULT_PTZ_DEVICE: &str = "/dev/motor";

/// Calibrated horizontal travel in degrees.
pub const PAN_MAX_DEGREES: i32 = 355;

/// Calibrated vertical travel in degrees.
pub const TILT_MAX_DEGREES: i32 = 75;

/// Calibrated horizontal park position.
pub const PAN_CENTER_DEGREES: i32 = 178;

/// Calibrated vertical park position.
pub const TILT_CENTER_DEGREES: i32 = 38;

/// Driver ioctl request values observed from `motor_drv.ko`.
const PTZ_IOC_INIT: u32 = 0x6d01;
const PTZ_IOC_GET_POS: u32 = 0x6d02;
const PTZ_IOC_MOVE: u32 = 0x6d03;
const PTZ_IOC_STOP: u32 = 0x6d05;

/// The driver accepts only these two timing selectors. Other values report a
/// move without energizing the coils.
const PTZ_SPEED_SLOW: i32 = 0;
const PTZ_SPEED_FAST: i32 = 1;

/// Caller speeds at or above this value use the driver's fast timing pair.
const PTZ_SPEED_FAST_THRESHOLD: u8 = 4;

/// Poll interval while a finite driver move reaches its target.
const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum time allowed for one finite axis move.
const POSITION_MOVE_TIMEOUT: Duration = Duration::from_secs(45);

/// Speed the park uses, and the speed a sweep travels at.
///
/// The park is the one movement where landing accuracy matters, and it is the
/// speed the centre button already used before the sweep existed. The legs run
/// fast: they end against a mechanical stop, so creeping towards one buys
/// nothing.
const PARK_SPEED: u8 = 5;
const SWEEP_SPEED: u8 = 10;

/// Vendor initialization parameters: angle limits, step rates, GPIO wiring,
/// and the four timing words recovered from the camera's driver path.
const VENDOR_INIT: [i32; 20] = [
    355, 75, 0, 0, 355, 75, 8533, 11377, 55, 59, 54, 58, 4, 57, 34, 56, 300, 300, 600, 400,
];

/// Direction understood by the local PTZ driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtzDirection {
    Left,
    Right,
    Up,
    Down,
    Stop,
}

impl PtzDirection {
    /// Parse the stable HTTP spelling used by the control API.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

impl std::fmt::Display for PtzDirection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
            Self::Stop => "stop",
        };
        formatter.write_str(value)
    }
}

/// Translate the public 0..10 speed scale to one of the two driver values.
pub const fn driver_speed(speed: u8) -> i32 {
    if speed >= PTZ_SPEED_FAST_THRESHOLD {
        PTZ_SPEED_FAST
    } else {
        PTZ_SPEED_SLOW
    }
}

/// Open, initialize, and serialize access to the camera PTZ driver.
#[derive(Debug)]
pub struct PtzController {
    file: Mutex<File>,
    generation: AtomicU64,
}

/// A timeout may stop only the continuous movement it actually started.
pub(crate) struct Motion {
    controller: Arc<PtzController>,
    generation: u64,
}

struct Calibration<'a> {
    generation: u64,
    cancelled: &'a dyn Fn() -> bool,
}

impl Calibration<'_> {
    fn check(&self, controller: &PtzController) -> io::Result<()> {
        if (self.cancelled)() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "PTZ calibration cancelled",
            ));
        }
        controller.check_generation(self.generation)
    }
}

impl Motion {
    pub fn stop(self) -> io::Result<()> {
        self.controller
            .command(None, Some(self.generation), "owned PTZ stop")
            .map(|_| ())
    }
}

impl PtzController {
    /// Open and initialize the camera's default PTZ device.
    pub fn open() -> io::Result<Self> {
        Self::open_path(DEFAULT_PTZ_DEVICE)
    }

    /// Open and initialize a caller-supplied device path.
    pub fn open_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let ptz = Self {
            file: Mutex::new(file),
            generation: AtomicU64::new(0),
        };
        ptz.initialize()?;
        info!(path = %path.display(), "PTZ controller initialized");
        Ok(ptz)
    }

    fn initialize(&self) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("PTZ device lock poisoned"))?;
        let result = unsafe { ioctl(file.as_raw_fd(), PTZ_IOC_INIT, VENDOR_INIT.as_ptr()) };
        check_ioctl(result, "PTZ initialization")
    }

    /// Return the native direction code used by the driver.
    pub const fn direction_code(direction: PtzDirection) -> Option<i32> {
        match direction {
            PtzDirection::Left => Some(1),
            PtzDirection::Right => Some(2),
            PtzDirection::Up => Some(3),
            PtzDirection::Down => Some(4),
            PtzDirection::Stop => None,
        }
    }

    /// Return whether a direction is already at its calibrated software limit.
    pub const fn at_limit(direction: PtzDirection, pan: i32, tilt: i32) -> bool {
        match direction {
            PtzDirection::Left => pan <= 0,
            PtzDirection::Right => pan >= PAN_MAX_DEGREES - 1,
            PtzDirection::Up => tilt <= 0,
            PtzDirection::Down => tilt >= TILT_MAX_DEGREES - 1,
            PtzDirection::Stop => true,
        }
    }

    /// Start continuous movement. Call [`Self::stop`] when the request ends.
    pub fn start_move(&self, direction: PtzDirection, speed: u8) -> io::Result<()> {
        self.start_generation(direction, speed).map(|_| ())
    }

    pub(crate) fn start_owned(
        self: &Arc<Self>,
        direction: PtzDirection,
        speed: u8,
    ) -> io::Result<Motion> {
        let generation = self.start_generation(direction, speed)?;
        Ok(Motion {
            controller: Arc::clone(self),
            generation,
        })
    }

    fn start_generation(&self, direction: PtzDirection, speed: u8) -> io::Result<u64> {
        if direction == PtzDirection::Stop {
            return self.command(None, None, "PTZ stop");
        }
        let direction_code = Self::direction_code(direction).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "stop is not a move direction")
        })?;
        let move_arg = [1_i32, direction_code, driver_speed(speed), -1_i32];
        self.command(Some(&move_arg), None, "continuous PTZ move")
    }

    /// Move by a bounded number of degrees, respecting the current software
    /// limit before issuing the driver request.
    pub fn move_step(&self, direction: PtzDirection, degrees: i32, speed: u8) -> io::Result<()> {
        if direction == PtzDirection::Stop {
            return self.stop();
        }
        if degrees <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTZ step must be positive",
            ));
        }
        let direction_code = Self::direction_code(direction).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "stop is not a move direction")
        })?;
        let (pan, tilt) = self.position()?;
        if Self::at_limit(direction, pan, tilt) {
            return self.stop();
        }
        let remaining = match direction {
            PtzDirection::Left => pan,
            PtzDirection::Right => PAN_MAX_DEGREES.saturating_sub(pan),
            PtzDirection::Up => tilt,
            PtzDirection::Down => TILT_MAX_DEGREES.saturating_sub(tilt),
            PtzDirection::Stop => 0,
        };
        let steps = degrees.min(remaining).max(1);
        let move_arg = [1_i32, direction_code, driver_speed(speed), steps];
        self.move_ioctl(&move_arg, "finite PTZ move")
    }

    fn move_ioctl(&self, move_arg: &[i32; 4], operation: &str) -> io::Result<()> {
        self.command(Some(move_arg), None, operation).map(|_| ())
    }

    /// Stop motion and invalidate an older calibration's remaining legs.
    pub fn stop(&self) -> io::Result<()> {
        self.command(None, None, "PTZ stop").map(|_| ())
    }

    fn check_generation(&self, expected: u64) -> io::Result<()> {
        if self.generation.load(Ordering::Acquire) != expected {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "PTZ centring superseded by another command",
            ));
        }
        Ok(())
    }

    fn command(
        &self,
        arguments: Option<&[i32; 4]>,
        expected: Option<u64>,
        operation: &str,
    ) -> io::Result<u64> {
        self.command_if(arguments, expected, operation, || false)
    }

    fn command_if(
        &self,
        arguments: Option<&[i32; 4]>,
        expected: Option<u64>,
        operation: &str,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<u64> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("PTZ device lock poisoned"))?;
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "PTZ calibration cancelled",
            ));
        }
        let generation = match expected {
            Some(generation) => {
                self.check_generation(generation)?;
                generation
            }
            None => self
                .generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1),
        };
        let request = if arguments.is_some() {
            PTZ_IOC_MOVE
        } else {
            PTZ_IOC_STOP
        };
        let arguments = arguments.map_or(std::ptr::null(), |arguments| arguments.as_ptr());
        // SAFETY: the live file and optional four-word driver argument remain
        // valid throughout ioctl. Token checks and commands share the same lock.
        let result = unsafe { ioctl(file.as_raw_fd(), request, arguments) };
        check_ioctl(result, operation)?;
        Ok(generation)
    }

    /// Read current position as `(pan, tilt)` degrees.
    pub fn position(&self) -> io::Result<(i32, i32)> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("PTZ device lock poisoned"))?;
        let mut position = [0_i32; 4];
        let result = unsafe { ioctl(file.as_raw_fd(), PTZ_IOC_GET_POS, position.as_mut_ptr()) };
        check_ioctl(result, "PTZ position read")?;
        Ok((position[2], position[3]))
    }

    /// Drive both axes onto both of their travel ends, then park at the centre.
    ///
    /// A bare park only moves the head to where the driver believes the centre is:
    /// the driver counts its own steps, and mechanical slack plus the one degree
    /// tolerance leave the head visibly off once it has been moved by hand or has
    /// lost steps. Touching both ends of each axis first gives the park a known
    /// reference, which is what makes centring repeatable.
    ///
    /// Measured on this camera: a full pan sweep takes 7.5 s at speed 10, and the
    /// whole sequence - left end, right end, up end, down end, park - lands within
    /// a degree of 178/38 in about thirty seconds.
    ///
    /// A newer move or stop cancels the remaining legs, including the park.
    pub fn center(&self) -> io::Result<()> {
        self.center_until(|| false)
    }

    pub(crate) fn center_until(&self, cancelled: impl Fn() -> bool) -> io::Result<()> {
        let calibration = Calibration {
            generation: self.command_if(None, None, "PTZ calibration stop", &cancelled)?,
            cancelled: &cancelled,
        };
        let (pan, _) = self.position()?;
        self.move_axis_to(
            pan,
            0,
            PtzDirection::Right,
            PtzDirection::Left,
            SWEEP_SPEED,
            &calibration,
        )?;
        let (pan, _) = self.position()?;
        self.move_axis_to(
            pan,
            PAN_MAX_DEGREES - 1,
            PtzDirection::Right,
            PtzDirection::Left,
            SWEEP_SPEED,
            &calibration,
        )?;
        let (_, tilt) = self.position()?;
        self.move_axis_to(
            tilt,
            0,
            PtzDirection::Down,
            PtzDirection::Up,
            SWEEP_SPEED,
            &calibration,
        )?;
        let (_, tilt) = self.position()?;
        self.move_axis_to(
            tilt,
            TILT_MAX_DEGREES - 1,
            PtzDirection::Down,
            PtzDirection::Up,
            SWEEP_SPEED,
            &calibration,
        )?;

        // The park keeps the speed it always used: the legs end against a
        // mechanical stop, but landing accuracy is the whole point of the park.
        let (pan, _) = self.position()?;
        self.move_axis_to(
            pan,
            PAN_CENTER_DEGREES,
            PtzDirection::Right,
            PtzDirection::Left,
            PARK_SPEED,
            &calibration,
        )?;
        let (_, tilt) = self.position()?;
        self.move_axis_to(
            tilt,
            TILT_CENTER_DEGREES,
            PtzDirection::Down,
            PtzDirection::Up,
            PARK_SPEED,
            &calibration,
        )?;
        Ok(())
    }

    fn move_axis_to(
        &self,
        current: i32,
        target: i32,
        positive_direction: PtzDirection,
        negative_direction: PtzDirection,
        speed: u8,
        calibration: &Calibration<'_>,
    ) -> io::Result<()> {
        calibration.check(self)?;
        if current == target {
            return Ok(());
        }
        let direction = if target > current {
            positive_direction
        } else {
            negative_direction
        };
        let direction = Self::direction_code(direction).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "calibration requires an axis direction",
            )
        })?;
        let arguments = [1, direction, driver_speed(speed), (target - current).abs()];
        self.command_if(
            Some(&arguments),
            Some(calibration.generation),
            "PTZ calibration move",
            calibration.cancelled,
        )?;
        let deadline = Instant::now() + POSITION_MOVE_TIMEOUT;
        loop {
            thread::sleep(POSITION_POLL_INTERVAL);
            calibration.check(self)?;
            let (pan, tilt) = self.position()?;
            let position = match positive_direction {
                PtzDirection::Left | PtzDirection::Right => pan,
                PtzDirection::Up | PtzDirection::Down => tilt,
                PtzDirection::Stop => current,
            };
            if (position - target).abs() <= 1 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = self.command_if(
                    None,
                    Some(calibration.generation),
                    "PTZ calibration timeout stop",
                    calibration.cancelled,
                );
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("PTZ did not reach {target} degrees; current position {position}"),
                ));
            }
        }
    }
}

impl Drop for PtzController {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn check_ioctl(result: i32, operation: &str) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::other(format!(
            "{operation} failed: {}",
            io::Error::last_os_error()
        )))
    } else {
        Ok(())
    }
}

// SAFETY: This is the stable POSIX ioctl ABI used by the preserved camera
// driver. Every call passes a valid descriptor and the argument layout
// documented next to the corresponding request.
unsafe extern "C" {
    fn ioctl(fd: i32, request: u32, ...) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direction_names() {
        assert_eq!(PtzDirection::parse("LEFT"), Some(PtzDirection::Left));
        assert_eq!(PtzDirection::parse("down"), Some(PtzDirection::Down));
        assert_eq!(PtzDirection::parse("diagonal"), None);
    }

    #[test]
    fn maps_driver_direction_codes() {
        assert_eq!(PtzController::direction_code(PtzDirection::Left), Some(1));
        assert_eq!(PtzController::direction_code(PtzDirection::Right), Some(2));
        assert_eq!(PtzController::direction_code(PtzDirection::Up), Some(3));
        assert_eq!(PtzController::direction_code(PtzDirection::Down), Some(4));
        assert_eq!(PtzController::direction_code(PtzDirection::Stop), None);
    }

    #[test]
    fn detects_calibrated_limits() {
        assert!(PtzController::at_limit(PtzDirection::Left, 0, 38));
        assert!(PtzController::at_limit(
            PtzDirection::Right,
            PAN_MAX_DEGREES - 1,
            38
        ));
        assert!(PtzController::at_limit(PtzDirection::Up, 178, 0));
        assert!(PtzController::at_limit(
            PtzDirection::Down,
            178,
            TILT_MAX_DEGREES - 1
        ));
        assert!(!PtzController::at_limit(PtzDirection::Right, 177, 38));
    }

    #[test]
    fn speed_mapping_only_emits_driver_values() {
        for speed in 0..=u8::MAX {
            assert!(matches!(
                driver_speed(speed),
                PTZ_SPEED_SLOW | PTZ_SPEED_FAST
            ));
        }
        assert_eq!(driver_speed(3), PTZ_SPEED_SLOW);
        assert_eq!(driver_speed(4), PTZ_SPEED_FAST);
    }

    // A superseded sweep must be rejected before issuing any ioctl, even if
    // its old timeout fires after another command has taken ownership.
    #[test]
    fn superseded_calibration_cannot_issue_a_move_or_timeout_stop() {
        let controller = PtzController {
            file: Mutex::new(File::open("/dev/null").unwrap()),
            generation: AtomicU64::new(1),
        };
        let arguments = [1, 1, 1, 1];
        assert_eq!(
            controller.center_until(|| true).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            controller
                .command(Some(&arguments), Some(0), "stale move")
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            controller
                .command(None, Some(0), "stale timeout")
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn an_old_hold_timeout_cannot_stop_a_newer_movement() {
        let controller = Arc::new(PtzController {
            file: Mutex::new(File::open("/dev/null").unwrap()),
            generation: AtomicU64::new(2),
        });
        let old_motion = Motion {
            controller: Arc::clone(&controller),
            generation: 1,
        };
        assert_eq!(
            old_motion.stop().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(controller.generation.load(Ordering::Acquire), 2);
    }

    #[test]
    fn nonexistent_device_returns_error() {
        assert!(PtzController::open_path("/tmp/c220-nonexistent-ptz").is_err());
    }
}
