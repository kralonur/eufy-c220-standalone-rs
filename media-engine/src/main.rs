//! Entry point of the media engine.
//!
//! On the camera this binary is `no_std`: Rust's `std` runtime always emits a
//! thread-local segment, and the board's uClibc loader faults on one in the main
//! executable, so the process starts through `__uClibc_main` with the stub in
//! the `camera` module and never touches `std`.
//!
//! On a development host the same engine runs unchanged with the mock backend,
//! which is how the daemon-facing protocol is exercised without hardware.

#![cfg_attr(target_arch = "arm", no_std)]
#![cfg_attr(target_arch = "arm", no_main)]

// Used by the command-line collector on the camera, where `std` is unavailable.
extern crate alloc;

#[cfg(target_arch = "arm")]
mod camera {
    //! The camera runtime: entry stub, allocator and panic handler.

    use c220_media_engine::backend::MockBackend;
    #[cfg(feature = "hardware")]
    use c220_media_engine::backend::{HardwareBackend, VideoConfig};
    use c220_media_engine::cli::{self, ConfigError};
    use c220_media_engine::{platform, run};
    use core::ffi::{CStr, c_char};
    use core::panic::PanicInfo;

    /// The engine's allocator. `std` is unavailable on this target, so the crate
    /// supplies its own `mmap`-backed implementation.
    #[global_allocator]
    static ALLOCATOR: platform::alloc::MmapAllocator = platform::alloc::MmapAllocator;

    // The engine's entry point.
    //
    // uClibc-ng has no `__libc_start_main`, so musl's CRT objects cannot be used.
    // This calls `__uClibc_main`, the uClibc equivalent, which performs the libc
    // initialization the vendor libraries rely on (thread pointer, malloc and
    // the executable's constructors). On entry `r0` holds the finalizer the
    // dynamic loader passes and the first stack word is `argc`.
    core::arch::global_asm!(
        r#"
        .syntax unified
        .arm
        .text
        .global _start
        .type _start,%function
    _start:
        mov r4, r0
        ldr r1, [sp]
        add r2, sp, #4
        mov r7, sp
        mov r3, #0
        mov r5, #0
        push {r5}
        push {r7}
        push {r4}
        push {r5}
        ldr r0, =engine_main
        bl __uClibc_main
        .word 0
        .size _start, .-_start
        "#,
        options(raw)
    );

    /// The engine's `main`, invoked by `__uClibc_main`.
    #[unsafe(no_mangle)]
    pub extern "C" fn engine_main(
        argc: core::ffi::c_int,
        argv: *const *const c_char,
    ) -> core::ffi::c_int {
        let config = match command_line(argc, argv)
            .and_then(|arguments| cli::parse_arguments(arguments).map_err(CommandLineError::Config))
        {
            Ok(config) => config,
            Err(error) => {
                platform::log(format_args!("media_arguments_error={error}"));
                return 2;
            }
        };

        // A build without the `hardware` feature is the one an emulator can run:
        // it links no vendor libraries and serves the protocol with the
        // hardware-free backend. The deployed build has them, and
        // `--mock-backend` is the diagnostic switch that keeps that build off the
        // devices while still letting the daemon talk to it.
        #[cfg(feature = "hardware")]
        if !config.mock_backend {
            let video = VideoConfig::new(
                &config.video_device,
                u32::try_from(config.video_fps).unwrap_or(1),
                u32::try_from(config.video_bitrate_kbps).unwrap_or(1),
            );
            let mut backend =
                HardwareBackend::new(&config.audio_card, config.audio_capture_enabled, video);
            let result = run(config, &mut backend);
            return match result {
                Ok(()) => 0,
                Err(error) => {
                    platform::log(format_args!("media_engine_error={error}"));
                    1
                }
            };
        }

        let mut backend = MockBackend::new();
        let result = run(config, &mut backend);
        match result {
            Ok(()) => 0,
            Err(error) => {
                platform::log(format_args!("media_engine_error={error}"));
                1
            }
        }
    }

    /// Why the command line could not be used.
    enum CommandLineError {
        Config(ConfigError),
        NotText,
    }

    impl core::fmt::Display for CommandLineError {
        fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::Config(error) => write!(formatter, "{error}"),
                Self::NotText => write!(formatter, "an argument is not valid UTF-8"),
            }
        }
    }

    /// Collect the arguments after `argv[0]`.
    ///
    /// The kernel leaves the command line on the initial stack, which lives as
    /// long as the process, so the borrowed strings stay valid.
    fn command_line(
        argc: core::ffi::c_int,
        argv: *const *const c_char,
    ) -> Result<alloc::vec::Vec<&'static str>, CommandLineError> {
        let mut arguments = alloc::vec::Vec::new();
        for index in 1..argc {
            // SAFETY: `__uClibc_main` passes the same `argc`/`argv` the kernel
            // handed the loader, so every index below `argc` is a valid pointer
            // to a NUL-terminated string on the initial stack.
            let pointer = unsafe { argv.add(index as usize) };
            let pointer = unsafe { *pointer };
            if pointer.is_null() {
                break;
            }
            let text = unsafe { CStr::from_ptr(pointer) };
            match text.to_str() {
                Ok(text) => arguments.push(text),
                Err(_) => return Err(CommandLineError::NotText),
            }
        }
        Ok(arguments)
    }

    /// The personality routine that precompiled `core` and `alloc` reference.
    ///
    /// Those crates ship compiled with unwinding, so code paths this binary uses,
    /// `alloc::fmt::format` among them, carry landing pads that name this symbol.
    /// Nothing here can reach one: the panic handler below aborts the process
    /// instead of unwinding, and `std` is not in the link to provide the
    /// definition it would otherwise contribute. Keeping it defined is what lets
    /// the binary link with an aborting panic strategy.
    #[unsafe(no_mangle)]
    extern "C" fn rust_eh_personality() {}

    /// Log the panic and stop.
    ///
    /// A supervisor restarts the engine, so the only thing this has to do is say
    /// why it died.
    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        let file = info
            .location()
            .map(|location| location.file())
            .unwrap_or("unknown");
        let line = info.location().map(|location| location.line()).unwrap_or(0);
        platform::log(format_args!(
            "media_panic={} at {file}:{line}",
            info.message()
        ));
        platform::exit(101)
    }
}

#[cfg(not(target_arch = "arm"))]
fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let config = match c220_media_engine::cli::parse_arguments(arguments.iter().map(String::as_str))
    {
        Ok(config) => config,
        Err(error) => {
            eprintln!("c220-media-engine: {error}");
            std::process::exit(2);
        }
    };
    println!(
        "c220-media-engine: development host build, mock backend, socket {}",
        config.socket_path
    );

    let mut backend = c220_media_engine::backend::MockBackend::new();
    if let Err(error) = c220_media_engine::run(config, &mut backend) {
        eprintln!("c220-media-engine: {error}");
        std::process::exit(1);
    }
}
