//! Rockchip AIQ: ISP tuning, and SmartIR for night detection.
//!
//! The AIQ context owns the ISP's 3A algorithms; SmartIR reads the statistics it
//! produces and decides whether the scene is dark enough for night mode, which
//! the reference engine then turns into the IR-cut and IR-LED state changes.

use crate::platform::Error;
use alloc::sync::Arc;
use core::ffi::{c_char, c_int, c_void};
use core::marker::PhantomData;
use core::mem::size_of;
use core::ptr;

/// Opaque ISP tuning context.
#[repr(C)]
pub struct Context {
    _private: [u8; 0],
}

/// Opaque night-detection context.
#[repr(C)]
pub struct SmartIrContext {
    _private: [u8; 0],
}

/// Size of the buffer the static-metadata call fills.
///
/// The reference engine declares 4096 bytes on the stack and reads the sensor
/// name out of the front of it.
pub const STATIC_META_LEN: usize = 4096;

/// Bytes of that buffer that hold the sensor entity name.
pub const SENSOR_NAME_LEN: usize = 32;

unsafe extern "C" {
    // The AIQ symbols carry a `CODEABI_3.8` version tag in `librkaiq.so`; the
    // default version is what a plain reference resolves to.
    fn rk_aiq_uapi2_sysctl_enumStaticMetas(index: c_int, static_info: *mut c_void) -> i32;
    fn rk_aiq_uapi2_sysctl_init(
        sensor: *const c_char,
        iq_directory: *const c_char,
        error_callback: *mut c_void,
        metas_callback: *mut c_void,
    ) -> *mut Context;
    fn rk_aiq_uapi2_sysctl_prepare(
        context: *const Context,
        width: u32,
        height: u32,
        mode: c_int,
    ) -> i32;
    fn rk_aiq_uapi2_sysctl_start(context: *const Context) -> i32;
    fn rk_aiq_uapi2_sysctl_stop(context: *const Context, keep_external_hardware: c_int) -> i32;
    fn rk_aiq_uapi2_sysctl_deinit(context: *mut Context);
    fn rk_aiq_uapi2_sysctl_switch_scene(
        context: *const Context,
        main_scene: *const c_char,
        sub_scene: *const c_char,
    ) -> i32;
    fn rk_aiq_uapi2_sysctl_get3AStatsBlk(
        context: *const Context,
        statistics: *mut *mut c_void,
        timeout_ms: c_int,
    ) -> i32;
    fn rk_aiq_uapi2_sysctl_release3AStatsRef(context: *const Context, statistics: *mut c_void);

    fn rk_smart_ir_init(context: *const Context) -> *mut SmartIrContext;
    fn rk_smart_ir_deInit(context: *mut SmartIrContext);
    fn rk_smart_ir_runOnce(
        context: *mut SmartIrContext,
        statistics: *mut c_void,
        is_night: *mut c_int,
    ) -> i32;
    fn rk_smart_ir_config(context: *mut SmartIrContext, config: *const SmartIrConfig) -> i32;
}

/// The threshold set the night detector applies to the statistics it is handed.
///
/// Six floats and a whole-sample counter, in this order and with no padding: the
/// library copies exactly seven words into its own context, which is why the
/// assertion below counts words rather than fields.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SmartIrConfig {
    /// Mean luma below this counts as night.
    pub day_to_night_luma: f32,
    /// Mean luma above this counts as day.
    pub night_to_day_luma: f32,
    /// AWB red gain of a frame lit by the infrared LED.
    pub rggain_base: f32,
    /// AWB blue gain of a frame lit by the infrared LED.
    pub bggain_base: f32,
    /// Radius around those gains that counts as "this frame is infrared".
    pub awbgain_radius: f32,
    /// Distance from those gains that counts as sunlight.
    pub awbgain_distance: f32,
    /// Samples the library confirms one verdict over.
    pub switch_samples: i32,
}

const _: () = assert!(size_of::<SmartIrConfig>() == 7 * size_of::<f32>());

/// The tuning this camera's sensor was calibrated with.
///
/// The library's own defaults are not these values, and one of them is actively
/// harmful here: with a non-zero AWB radius the set of blocks it averages can come
/// up empty in a monochrome infrared frame, so it measures the origin against the
/// baseline gains, reads daylight while the IR LED is on, and flips the LED off -
/// which drops the frame back to dark and repeats all night. These are the values
/// the factory application applies in its auto branch, read out of its
/// `anker_t8w11` binary, where each is stored as an integer scaled by 10000.
pub const FACTORY_TUNING: SmartIrConfig = SmartIrConfig {
    day_to_night_luma: 0.0328, // 328/10000
    night_to_day_luma: 0.0623, // 623/10000
    rggain_base: 1.0358,       // 10358/10000
    bggain_base: 1.0082,       // 10082/10000
    awbgain_radius: 0.0,       // 0/10000: weigh every block, so the set is never empty
    awbgain_distance: 0.2,     // 2000/10000
    switch_samples: 1,         // 10000/10000: the library then confirms over two itself
};

/// Read the sensor entity name AIQ expects for its first metadata index.
pub fn sensor_name() -> Result<alloc::ffi::CString, Error> {
    let mut meta = [0_u8; STATIC_META_LEN];
    super::check(unsafe {
        rk_aiq_uapi2_sysctl_enumStaticMetas(0, meta.as_mut_ptr() as *mut c_void)
    })?;

    // The name is NUL-padded inside the first 32 bytes; the reference engine
    // copies all 32 and relies on the terminator.
    let field = &meta[..SENSOR_NAME_LEN];
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if end == 0 {
        return Err(Error::from_errno(libc::ENODEV));
    }
    alloc::ffi::CString::new(&field[..end]).map_err(|_| Error::from_errno(libc::EINVAL))
}

/// The ISP tuning context.
pub struct Aiq {
    context: *mut Context,
    started: bool,
}

impl Aiq {
    /// Open the tuning context for a sensor, reading its calibration from
    /// `iq_directory`.
    pub fn open(sensor: &core::ffi::CStr, iq_directory: &core::ffi::CStr) -> Result<Self, Error> {
        let context = unsafe {
            rk_aiq_uapi2_sysctl_init(
                sensor.as_ptr(),
                iq_directory.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if context.is_null() {
            return Err(Error::from_errno(libc::ENODEV));
        }
        Ok(Self {
            context,
            started: false,
        })
    }

    /// Configure the pipeline for a capture resolution.
    pub fn prepare(&mut self, width: u32, height: u32, mode: c_int) -> Result<(), Error> {
        super::check(unsafe { rk_aiq_uapi2_sysctl_prepare(self.context, width, height, mode) })
    }

    /// Start the ISP and its 3A loops.
    pub fn start(&mut self) -> Result<(), Error> {
        super::check(unsafe { rk_aiq_uapi2_sysctl_start(self.context) })?;
        self.started = true;
        Ok(())
    }

    /// Switch the tuning scene, which is how night mode changes the image.
    pub fn switch_scene(
        &self,
        main_scene: &core::ffi::CStr,
        sub_scene: &core::ffi::CStr,
    ) -> Result<(), Error> {
        super::check(unsafe {
            rk_aiq_uapi2_sysctl_switch_scene(self.context, main_scene.as_ptr(), sub_scene.as_ptr())
        })
    }

    /// Wait for the next block of 3A statistics.
    ///
    /// `Ok(None)` means the wait timed out without producing a block, which the
    /// reference engine treats as "nothing to decide this second".
    pub fn statistics(&self, timeout_ms: c_int) -> Result<Option<Statistics<'_>>, Error> {
        let mut pointer: *mut c_void = ptr::null_mut();
        let status =
            unsafe { rk_aiq_uapi2_sysctl_get3AStatsBlk(self.context, &mut pointer, timeout_ms) };
        if status != super::RK_SUCCESS || pointer.is_null() {
            return Ok(None);
        }
        Ok(Some(Statistics {
            context: self.context,
            pointer,
            owner: PhantomData,
        }))
    }
}

// SAFETY: the context is a vendor handle used through `&self` (`switch_scene` and
// `statistics`) or `&mut self` (`prepare` and `start`, which run before it is ever
// shared). The engine hands it to the night monitor and keeps no other user: the
// capture thread only created it, so at most one thread calls into it at a time.
unsafe impl Send for Aiq {}
unsafe impl Sync for Aiq {}

impl Drop for Aiq {
    fn drop(&mut self) {
        if self.started {
            unsafe { rk_aiq_uapi2_sysctl_stop(self.context, 0) };
        }
        unsafe { rk_aiq_uapi2_sysctl_deinit(self.context) };
    }
}

impl core::fmt::Debug for Aiq {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Aiq")
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

/// One block of 3A statistics, returned to AIQ when dropped.
pub struct Statistics<'a> {
    context: *const Context,
    pointer: *mut c_void,
    owner: PhantomData<&'a Aiq>,
}

impl Statistics<'_> {
    /// The vendor statistics block, for handing to SmartIR.
    pub fn pointer(&self) -> *mut c_void {
        self.pointer
    }
}

impl Drop for Statistics<'_> {
    fn drop(&mut self) {
        unsafe { rk_aiq_uapi2_sysctl_release3AStatsRef(self.context, self.pointer) };
    }
}

/// The night detector AIQ ships with.
///
/// It holds its share of the tuning context rather than borrowing it, because the
/// thread that reads the statistics is not the thread that started the ISP.
pub struct SmartIr {
    context: *mut SmartIrContext,
    aiq: Arc<Aiq>,
}

// SAFETY: as for `Aiq`: the detector is used by the night monitor alone, and the
// context it keeps alive is the one the library attached it to.
unsafe impl Send for SmartIr {}

impl SmartIr {
    /// Attach the detector to an open AIQ context.
    ///
    /// `Ok(None)` means the library had no detector for this sensor, which the
    /// reference engine treats as "no automatic night mode" rather than a
    /// failure to start.
    pub fn open(aiq: Arc<Aiq>) -> Result<Option<Self>, Error> {
        let context = unsafe { rk_smart_ir_init(aiq.context) };
        if context.is_null() {
            return Ok(None);
        }
        Ok(Some(Self { context, aiq }))
    }

    /// The tuning context this detector reads statistics from.
    pub fn tuning(&self) -> &Aiq {
        &self.aiq
    }

    /// Decide whether the scene in `statistics` is dark enough for night mode.
    ///
    /// `current` is the mode the hardware is in, which the library takes as its
    /// starting opinion - the reference engine seeds it from its own night state
    /// for the same reason.
    pub fn is_night(&mut self, statistics: &Statistics<'_>, current: bool) -> Result<bool, Error> {
        let mut is_night: c_int = current as c_int;
        super::check(unsafe {
            rk_smart_ir_runOnce(self.context, statistics.pointer(), &mut is_night)
        })?;
        Ok(is_night != 0)
    }

    /// Apply a threshold set.
    ///
    /// Without this the library runs on defaults that are not this sensor's; see
    /// [`FACTORY_TUNING`].
    pub fn configure(&mut self, config: &SmartIrConfig) -> Result<(), Error> {
        super::check(unsafe { rk_smart_ir_config(self.context, config) })
    }
}

impl Drop for SmartIr {
    fn drop(&mut self) {
        unsafe { rk_smart_ir_deInit(self.context) };
    }
}

impl core::fmt::Debug for SmartIr {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("SmartIr")
    }
}
