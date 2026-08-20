//! Camera-local PTZ motor control through the preserved `/dev/motor` driver.
//!
//! The ioctl numbers, initialization array, axis limits, GPIO assignments, and
//! speed mapping are reverse-engineered from the C220's `motor_drv.ko` path.
//! This module does not use the vendor/P2P protocol.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use tracing::info;

/// Default PTZ character device exposed by the camera kernel module.
pub const DEFAULT_MOTOR_DEVICE: &str = "/dev/motor";

/// Calibrated horizontal travel in degrees.
pub const PAN_MAX_DEGREES: i32 = 355;

/// Calibrated vertical travel in degrees.
pub const TILT_MAX_DEGREES: i32 = 75;

/// Calibrated horizontal park position.
pub const PAN_CENTER_DEGREES: i32 = 178;

/// Calibrated vertical park position.
pub const TILT_CENTER_DEGREES: i32 = 38;

/// Driver ioctl request values observed from `motor_drv.ko`.
const MOTOR_IOC_INIT: u32 = 0x6d01;
const MOTOR_IOC_GET_POS: u32 = 0x6d02;
const MOTOR_IOC_MOVE: u32 = 0x6d03;
const MOTOR_IOC_STOP: u32 = 0x6d05;

/// The driver accepts only these two timing selectors. Other values report a
/// move without energizing the coils.
const MOTOR_SPEED_SLOW: i32 = 0;
const MOTOR_SPEED_FAST: i32 = 1;

/// Caller speeds at or above this value use the driver's fast timing pair.
const MOTOR_SPEED_FAST_THRESHOLD: u8 = 4;

/// Poll interval while a finite driver move reaches its target.
const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum time allowed for one finite axis move.
const POSITION_MOVE_TIMEOUT: Duration = Duration::from_secs(45);

/// Vendor initialization parameters: angle limits, step rates, GPIO wiring,
/// and the four timing words recovered from the camera's driver path.
const VENDOR_INIT: [i32; 20] = [
    355, 75, 0, 0, 355, 75, 8533, 11377, 55, 59, 54, 58, 4, 57, 34, 56, 300, 300, 600, 400,
];

/// Direction understood by the local motor driver.
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
    if speed >= MOTOR_SPEED_FAST_THRESHOLD {
        MOTOR_SPEED_FAST
    } else {
        MOTOR_SPEED_SLOW
    }
}

/// Open, initialize, and serialize access to the camera motor driver.
#[derive(Debug)]
pub struct HardwareMotor {
    file: Mutex<File>,
}

impl HardwareMotor {
    /// Open and initialize the camera's default motor device.
    pub fn open() -> io::Result<Self> {
        Self::open_path(DEFAULT_MOTOR_DEVICE)
    }

    /// Open and initialize a caller-supplied device path.
    pub fn open_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let motor = Self {
            file: Mutex::new(file),
        };
        motor.initialize()?;
        info!(path = %path.display(), "hardware PTZ motor initialized");
        Ok(motor)
    }

    fn initialize(&self) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("motor device lock poisoned"))?;
        let result = unsafe { ioctl(file.as_raw_fd(), MOTOR_IOC_INIT, VENDOR_INIT.as_ptr()) };
        check_ioctl(result, "motor initialization")
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
        if direction == PtzDirection::Stop {
            return self.stop();
        }
        let direction_code = Self::direction_code(direction).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "stop is not a move direction")
        })?;
        let move_arg = [1_i32, direction_code, driver_speed(speed), -1_i32];
        self.move_ioctl(&move_arg, "continuous motor move")
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
                "motor step must be positive",
            ));
        }
        let direction_code = Self::direction_code(direction).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "stop is not a move direction")
        })?;
        let (pan, tilt) = self.position()?;
        if Self::at_limit(direction, pan, tilt) {
            return Ok(());
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
        self.move_ioctl(&move_arg, "finite motor move")
    }

    fn move_ioctl(&self, move_arg: &[i32; 4], operation: &str) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("motor device lock poisoned"))?;
        let result = unsafe { ioctl(file.as_raw_fd(), MOTOR_IOC_MOVE, move_arg.as_ptr()) };
        check_ioctl(result, operation)
    }

    /// Stop all motor motion.
    pub fn stop(&self) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("motor device lock poisoned"))?;
        let result = unsafe { ioctl(file.as_raw_fd(), MOTOR_IOC_STOP, 0) };
        check_ioctl(result, "motor stop")
    }

    /// Read current position as `(pan, tilt)` degrees.
    pub fn position(&self) -> io::Result<(i32, i32)> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("motor device lock poisoned"))?;
        let mut position = [0_i32; 4];
        let result = unsafe { ioctl(file.as_raw_fd(), MOTOR_IOC_GET_POS, position.as_mut_ptr()) };
        check_ioctl(result, "motor position read")?;
        Ok((position[2], position[3]))
    }

    /// Move both axes to the calibrated center and wait for the driver to
    /// report the target. This is a park operation, not a physical homing
    /// sweep; it avoids moving blindly when the camera was moved by hand.
    pub fn center(&self, speed: u8) -> io::Result<()> {
        self.stop()?;
        let (pan, _) = self.position()?;
        self.move_axis_to(
            pan,
            PAN_CENTER_DEGREES,
            PtzDirection::Right,
            PtzDirection::Left,
            speed,
        )?;
        let (_, tilt) = self.position()?;
        self.move_axis_to(
            tilt,
            TILT_CENTER_DEGREES,
            PtzDirection::Down,
            PtzDirection::Up,
            speed,
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
    ) -> io::Result<()> {
        if current == target {
            return Ok(());
        }
        let direction = if target > current {
            positive_direction
        } else {
            negative_direction
        };
        self.move_step(direction, (target - current).abs(), speed)?;
        let deadline = Instant::now() + POSITION_MOVE_TIMEOUT;
        loop {
            thread::sleep(POSITION_POLL_INTERVAL);
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
                let _ = self.stop();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("motor did not reach {target} degrees; current position {position}"),
                ));
            }
        }
    }
}

impl Drop for HardwareMotor {
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
        assert_eq!(HardwareMotor::direction_code(PtzDirection::Left), Some(1));
        assert_eq!(HardwareMotor::direction_code(PtzDirection::Right), Some(2));
        assert_eq!(HardwareMotor::direction_code(PtzDirection::Up), Some(3));
        assert_eq!(HardwareMotor::direction_code(PtzDirection::Down), Some(4));
        assert_eq!(HardwareMotor::direction_code(PtzDirection::Stop), None);
    }

    #[test]
    fn detects_calibrated_limits() {
        assert!(HardwareMotor::at_limit(PtzDirection::Left, 0, 38));
        assert!(HardwareMotor::at_limit(
            PtzDirection::Right,
            PAN_MAX_DEGREES - 1,
            38
        ));
        assert!(HardwareMotor::at_limit(PtzDirection::Up, 178, 0));
        assert!(HardwareMotor::at_limit(
            PtzDirection::Down,
            178,
            TILT_MAX_DEGREES - 1
        ));
        assert!(!HardwareMotor::at_limit(PtzDirection::Right, 177, 38));
    }

    #[test]
    fn speed_mapping_only_emits_driver_values() {
        for speed in 0..=u8::MAX {
            assert!(matches!(
                driver_speed(speed),
                MOTOR_SPEED_SLOW | MOTOR_SPEED_FAST
            ));
        }
        assert_eq!(driver_speed(3), MOTOR_SPEED_SLOW);
        assert_eq!(driver_speed(4), MOTOR_SPEED_FAST);
    }

    #[test]
    fn nonexistent_device_returns_error() {
        assert!(HardwareMotor::open_path("/tmp/c220-nonexistent-motor").is_err());
    }
}
