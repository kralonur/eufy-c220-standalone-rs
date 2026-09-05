//! Sysfs GPIO output pins.
//!
//! The amplifier enable line is one, and the IR-cut and IR-LED lines that night
//! mode drives are two more, so the sequence lives here once: export the pin if
//! the kernel has not already, make sure its direction is `out`, and write the
//! value. What this module exported, it unexports again - a pin the firmware or
//! the factory setup already exported is left alone, exactly as the reference
//! engine does it.
//!
//! The class path is a parameter rather than a constant so the sequence can be
//! exercised against a temporary directory on a development host, where there is
//! no GPIO class at all.

use crate::platform::file;
use crate::platform::{self, Error};
use alloc::format;
use alloc::string::String;

/// Where the kernel exposes the GPIO class on the camera.
pub const CLASS_PATH: &str = "/sys/class/gpio";

/// Pause after exporting a pin, in microseconds.
///
/// The kernel creates a pin's attributes asynchronously, so the reference engine
/// waits here too before touching the direction.
const EXPORT_SETTLE_US: u32 = 10_000;

/// An output pin the engine drives.
pub struct OutputPin {
    number: u32,
    class_path: String,
    exported_here: bool,
    high: bool,
}

impl OutputPin {
    /// Open a pin at the camera's standard class path.
    pub fn open(number: u32) -> Result<Self, Error> {
        Self::open_in(CLASS_PATH, number)
    }

    /// Open a pin below a given GPIO class path.
    pub fn open_in(class_path: &str, number: u32) -> Result<Self, Error> {
        let mut pin = Self {
            number,
            class_path: String::from(class_path),
            exported_here: false,
            high: false,
        };
        pin.ensure_exported()?;
        // Start with the pin driven low, as the reference engine does: the
        // amplifier must not be enabled before the audio path is configured.
        pin.set(false)?;
        Ok(pin)
    }

    /// The pin number.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// Whether the pin is currently driven high.
    pub fn is_high(&self) -> bool {
        self.high
    }

    /// Drive the pin high unless it already is.
    ///
    /// The playback path arms the amplifier once per burst rather than on every
    /// block, which is what the reference engine's `!speaker_gpio_active` check
    /// amounts to.
    pub fn set_if_low(&mut self) -> Result<(), Error> {
        if self.high { Ok(()) } else { self.set(true) }
    }

    /// Drive the pin high or low.
    pub fn set(&mut self, high: bool) -> Result<(), Error> {
        file::write_text(
            &format!("{}/gpio{}/direction", self.class_path, self.number),
            "out",
        )?;
        file::write_text(
            &format!("{}/gpio{}/value", self.class_path, self.number),
            if high { "1" } else { "0" },
        )?;
        self.high = high;
        Ok(())
    }

    /// Export the pin unless it is already there.
    fn ensure_exported(&mut self) -> Result<(), Error> {
        if file::exists(&format!("{}/gpio{}", self.class_path, self.number)) {
            return Ok(());
        }
        file::write_text(
            &format!("{}/export", self.class_path),
            &format!("{}", self.number),
        )?;
        self.exported_here = true;
        platform::sleep_us(EXPORT_SETTLE_US);
        Ok(())
    }
}

impl Drop for OutputPin {
    fn drop(&mut self) {
        if self.high {
            let _ = self.set(false);
        }
        if self.exported_here {
            let _ = file::write_text(
                &format!("{}/unexport", self.class_path),
                &format!("{}", self.number),
            );
        }
    }
}

impl core::fmt::Debug for OutputPin {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("OutputPin")
            .field("number", &self.number)
            .field("high", &self.high)
            .field("exported_here", &self.exported_here)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A temporary stand-in for `/sys/class/gpio`.
    struct FakeClass {
        path: PathBuf,
    }

    impl FakeClass {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("c220-gpio-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp class should be creatable");
            // The kernel always provides these two files in the class directory,
            // and the engine opens them without creating them.
            fs::write(path.join("export"), "").expect("export file");
            fs::write(path.join("unexport"), "").expect("unexport file");
            Self { path }
        }

        fn path(&self) -> &str {
            self.path.to_str().expect("temp path is UTF-8")
        }

        /// Emulate the kernel creating a pin that was just exported.
        fn create_pin(&self, number: u32) {
            let pin = self.path.join(format!("gpio{number}"));
            fs::create_dir_all(&pin).expect("pin directory");
            fs::write(pin.join("direction"), "").expect("direction file");
            fs::write(pin.join("value"), "").expect("value file");
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.path.join(relative)).expect("file should be readable")
        }

        fn is_empty(&self, relative: &str) -> bool {
            self.read(relative).is_empty()
        }
    }

    impl Drop for FakeClass {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn drives_an_already_exported_pin_and_leaves_the_export_alone() {
        let class = FakeClass::new("existing");
        class.create_pin(102);

        let mut pin = OutputPin::open_in(class.path(), 102).expect("pin should open");
        assert_eq!(pin.number(), 102);
        assert!(!pin.is_high());
        assert_eq!(class.read("gpio102/direction"), "out");
        assert_eq!(class.read("gpio102/value"), "0");

        pin.set(true).expect("pin should go high");
        assert!(pin.is_high());
        assert_eq!(class.read("gpio102/value"), "1");

        drop(pin);
        // Cleanup drives the pin low and does not unexport what it did not export.
        assert_eq!(class.read("gpio102/value"), "0");
        assert!(class.is_empty("unexport"));
    }

    #[test]
    fn exports_a_missing_pin_and_unexports_it_again() {
        let class = FakeClass::new("missing");

        // Nothing emulates the kernel creating the pin's attributes here, so the
        // sequence stops after the export - which is what this test is looking
        // at. On the camera the attributes appear and the same call succeeds.
        let result = OutputPin::open_in(class.path(), 102);
        assert!(
            result.is_err(),
            "an attribute write cannot succeed without a pin"
        );
        assert_eq!(class.read("export"), "102");
        assert_eq!(class.read("unexport"), "102");
    }

    #[test]
    fn a_pin_high_at_drop_is_driven_low_first() {
        let class = FakeClass::new("drop");
        class.create_pin(102);

        let mut pin = OutputPin::open_in(class.path(), 102).expect("pin should open");
        pin.set(true).expect("pin should go high");
        drop(pin);

        assert_eq!(class.read("gpio102/value"), "0");
        assert_eq!(class.read("gpio102/direction"), "out");
    }
}
