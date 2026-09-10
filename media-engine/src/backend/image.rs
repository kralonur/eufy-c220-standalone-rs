//! The image path: ISP tuning, and the night mode that follows the light.
//!
//! Ported from the reference engine's `start_aiq`, `stop_aiq`,
//! `ircut_switch`, `apply_night_mode` and `night_monitor_thread`. Three things
//! about it are worth knowing before reading the code:
//!
//! * **`HDR_MODE` has to be set before AIQ loads its tuning files.** It selects
//!   which IQ files the library reads (`_normal` rather than `_hdr3`), the
//!   factory engine sets it in the same place, and it is why the environment is
//!   touched here rather than at process start.
//! * **The library decides, the engine confirms.** One verdict is one block of
//!   statistics wide, so a change has to hold for several samples and the sensor
//!   is then left alone while the AE and the IR-cut settle - otherwise a passing
//!   headlight or the engine's own LED flips the mode back and forth.
//! * **A commanded mode owns the hardware.** While the daemon has asked for night
//!   or day, the detector is not consulted at all, so its opinion cannot fight the
//!   command.
//!
//! All of the hardware switching happens on the monitor thread. The daemon's
//! commands only record what was asked for, which keeps the IR-cut, the IR LED and
//! the AIQ scene under one thread's ownership instead of two.

use super::{Detector, NightState, Step, Verdict};
use crate::client::RecordSink;
use crate::gpio::OutputPin;
use crate::platform::signal::Shutdown;
use crate::platform::sync::Mutex;
use crate::platform::thread::{self, Thread};
use crate::platform::{Error, media_log, sleep_us};
use crate::vendor::aiq::{self, Aiq, FACTORY_TUNING, SmartIr};
use alloc::ffi::CString;
use alloc::sync::Arc;

/// The IR-cut filter's two coil pins, from the reference engine.
const IRCUT_PIN_A: u32 = 130;
/// The other coil of the same filter.
const IRCUT_PIN_B: u32 = 134;
/// The infrared illumination pin.
const IR_LED_PIN: u32 = 118;

/// How long the IR-cut coil is driven, in microseconds.
///
/// The filter is a small mechanical part: the coil moves it and then has to be
/// released, which is what this delay is for.
const IRCUT_PULSE_US: u32 = 200_000;

/// How long the monitor sleeps between statistics, in microseconds (1 Hz).
const MONITOR_PERIOD_US: u32 = 1_000_000;

/// How long the statistics call waits for a block, in milliseconds.
const STATISTICS_TIMEOUT_MS: i32 = 500;

/// Capture size the ISP is prepared for, which the video path also captures.
const WIDTH: u32 = super::video::WIDTH;
/// Capture height the ISP is prepared for.
const HEIGHT: u32 = super::video::HEIGHT;

/// Name the monitor thread carries in `/proc`.
const NIGHT_THREAD: &str = "c220-night";

/// The AIQ scene names of a day/night switch, from the reference engine.
const MAIN_SCENE: &str = "normal";
/// Sub-scene for daylight.
const DAY_SCENE: &str = "day";
/// Sub-scene for infrared night mode.
const NIGHT_SCENE: &str = "night";

/// Describes the tuning files the library should read, which is a linear sensor
/// mode rather than any of the HDR ones.
const HDR_MODE_VARIABLE: &str = "HDR_MODE";

/// Linear mode, which is what the factory engine selects too.
const HDR_MODE_LINEAR: &str = "0";

/// The ISP tuning context and the night detector attached to it.
pub struct ImagePath {
    aiq: Arc<Aiq>,
    smart_ir: Option<SmartIr>,
}

impl ImagePath {
    /// Start the ISP for this sensor, with the night detector armed.
    ///
    /// `iq_directory` is where AIQ reads the calibration files the camera ships.
    pub fn open(iq_directory: &str) -> Result<Self, Error> {
        set_hdr_mode()?;
        let sensor = aiq::sensor_name()?;
        let directory = CString::new(iq_directory).map_err(|_| Error::from_errno(libc::EINVAL))?;
        let mut context = Aiq::open(&sensor, &directory)?;
        media_log!(
            "media_aiq_enum=0 sensor='{}'",
            sensor.to_string_lossy().as_ref()
        );
        context.prepare(WIDTH, HEIGHT, 0)?;
        context.start()?;
        let aiq = Arc::new(context);

        let smart_ir = SmartIr::open(Arc::clone(&aiq))?;
        let smart_ir = match smart_ir {
            Some(mut detector) => {
                if let Err(error) = detector.configure(&FACTORY_TUNING) {
                    // The library's defaults are not this sensor's, and one of them
                    // makes an infrared frame look like daylight - which is the
                    // hunting this configuration exists to prevent.
                    media_log!("media_smart_ir_config_failed={error}");
                }
                media_log!("media_smart_ir_ready=1");
                Some(detector)
            }
            None => {
                media_log!("media_smart_ir_init_failed=1");
                None
            }
        };

        Ok(Self { aiq, smart_ir })
    }

    /// The tuning context, for scene switches.
    pub fn tuning(&self) -> &Aiq {
        &self.aiq
    }

    /// One sample from the detector, or `Unknown` when there is nothing to read.
    ///
    /// A missing detector or a statistics call that times out is not an error: the
    /// reference engine simply waits for the next second.
    pub fn sample(&mut self, current: bool) -> Verdict {
        let Some(detector) = self.smart_ir.as_mut() else {
            return Verdict::Unknown;
        };
        let Ok(Some(statistics)) = self.aiq.statistics(STATISTICS_TIMEOUT_MS) else {
            return Verdict::Unknown;
        };
        match detector.is_night(&statistics, current) {
            Ok(true) => Verdict::Night,
            Ok(false) => Verdict::Day,
            Err(error) => {
                media_log!("media_smart_ir_error={error}");
                Verdict::Unknown
            }
        }
    }
}

/// Select the linear sensor mode before AIQ reads its tuning files.
fn set_hdr_mode() -> Result<(), Error> {
    let name = CString::new(HDR_MODE_VARIABLE).map_err(|_| Error::from_errno(libc::EINVAL))?;
    let value = CString::new(HDR_MODE_LINEAR).map_err(|_| Error::from_errno(libc::EINVAL))?;
    if unsafe { libc::setenv(name.as_ptr(), value.as_ptr(), 1) } != 0 {
        return Err(Error::last());
    }
    Ok(())
}

/// The night hardware: the IR-cut filter, the infrared LED and the IR scene.
struct NightHardware {
    ircut_a: OutputPin,
    ircut_b: OutputPin,
    ir_led: OutputPin,
    /// The mode the hardware is in, as far as this value knows.
    night: bool,
}

impl NightHardware {
    /// Take the three pins and leave the filter where the camera started.
    fn open() -> Result<Self, Error> {
        let mut ircut_a = OutputPin::open(IRCUT_PIN_A)?;
        let mut ircut_b = OutputPin::open(IRCUT_PIN_B)?;
        let mut ir_led = OutputPin::open(IR_LED_PIN)?;
        // Day: the filter is in place, illumination off - the reference engine's
        // starting state.
        pulse(&mut ircut_a, &mut ircut_b, false)?;
        ir_led.set(false)?;
        Ok(Self {
            ircut_a,
            ircut_b,
            ir_led,
            night: false,
        })
    }

    /// Move the filter and the illumination to the requested mode.
    fn apply(&mut self, night: bool) -> Result<(), Error> {
        if night == self.night {
            return Ok(());
        }
        pulse(&mut self.ircut_a, &mut self.ircut_b, night)?;
        self.ir_led.set(night)?;
        self.night = night;
        Ok(())
    }
}

/// Drive the IR-cut filter to one end and release the coil.
fn pulse(ircut_a: &mut OutputPin, ircut_b: &mut OutputPin, night: bool) -> Result<(), Error> {
    if night {
        // Night: the filter is pulled out of the light path by the other coil.
        ircut_b.set(false)?;
        ircut_a.set(true)?;
        sleep_us(IRCUT_PULSE_US);
        ircut_a.set(false)?;
        ircut_b.set(false)?;
    } else {
        ircut_a.set(false)?;
        ircut_b.set(true)?;
        sleep_us(IRCUT_PULSE_US);
        ircut_a.set(false)?;
        ircut_b.set(false)?;
    }
    Ok(())
}

/// The night monitor thread and the hardware it drives.
pub struct NightMonitor {
    thread: Thread,
}

impl NightMonitor {
    /// Start the monitor.
    ///
    /// The night state is shared with the session thread, which is where the
    /// daemon's commands land: the monitor reads the target from it on every tick
    /// and writes back the mode it put the hardware into.
    pub fn start(
        mut image: ImagePath,
        sink: Arc<RecordSink>,
        shutdown: Arc<Shutdown>,
        state: Arc<Mutex<NightState>>,
    ) -> Result<Self, Error> {
        let mut hardware = NightHardware::open()?;
        let mut detector = Detector::new();
        let thread = thread::spawn(NIGHT_THREAD, move || {
            let mut current = hardware.night;
            while !shutdown.requested() {
                sleep_us(MONITOR_PERIOD_US);
                if shutdown.requested() {
                    break;
                }

                let commanded = match state.lock() {
                    Ok(state) => state.commanded(),
                    Err(error) => {
                        media_log!("media_night_state_error={error}");
                        None
                    }
                };
                let verdict = if commanded.is_some() {
                    // A commanded mode owns the hardware, so the library is not
                    // consulted and its verdict cannot fight the command.
                    Verdict::Unknown
                } else {
                    image.sample(current)
                };

                match detector.step(commanded, current, verdict) {
                    Step::Hold => {}
                    Step::Switch(night) => {
                        if let Err(error) = hardware.apply(night) {
                            media_log!("media_night_hardware_error={error}");
                            continue;
                        }
                        current = night;
                        if let Err(error) = switch_scene(image.tuning(), night) {
                            // The IQ file has to carry both sub-scenes; a failure
                            // here leaves the day exposure profile in place all
                            // night, which looks like a bad night image rather
                            // than a rejected scene name.
                            media_log!("media_night_scene_error={error}");
                        }
                        media_log!("media_night_mode_switch={}", u8::from(night));
                        match state.lock() {
                            Ok(mut state) => {
                                let (night, manual) = {
                                    state.set(night);
                                    state.reported()
                                };
                                if let Err(error) = sink.send_night_mode(night, manual) {
                                    media_log!("media_night_report_error={error}");
                                }
                            }
                            Err(error) => media_log!("media_night_state_error={error}"),
                        }
                    }
                }
            }
            // Dropping the pins drives the illumination off; the filter is left
            // where it is, as the reference engine leaves it.
            drop(hardware);
        })?;
        Ok(Self { thread })
    }
}

impl Drop for NightMonitor {
    fn drop(&mut self) {
        self.thread.join();
    }
}

/// Tell AIQ which day/night tuning to use.
fn switch_scene(tuning: &Aiq, night: bool) -> Result<(), Error> {
    let main = CString::new(MAIN_SCENE).map_err(|_| Error::from_errno(libc::EINVAL))?;
    let sub = CString::new(if night { NIGHT_SCENE } else { DAY_SCENE })
        .map_err(|_| Error::from_errno(libc::EINVAL))?;
    tuning.switch_scene(&main, &sub)
}
