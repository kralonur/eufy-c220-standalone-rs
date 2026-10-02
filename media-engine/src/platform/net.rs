//! `AF_UNIX` stream sockets: the one private socket the firmware connects to.
//!
//! The socket is created with mode `0600` and the previous socket file is
//! unlinked first, so a crashed engine cannot block startup - both are what the
//! reference engine does.

use super::Error;
use alloc::ffi::CString;
use core::ffi::{c_int, c_void};
use core::mem::{self, size_of};
use core::ptr;

/// Length of `sun_path`, including the terminating NUL.
const SUN_PATH_LEN: usize = 108;

/// The firmware is the only client; the reference engine listens with this
/// backlog.
const BACKLOG: c_int = 2;

/// Socket file mode: root only.
const SOCKET_MODE: libc::mode_t = 0o600;

/// `struct timeval` as the *kernel* expects it for `SO_RCVTIMEO`.
///
/// Kernel socket timeouts use the kernel's own word size: 32-bit seconds on a
/// 32-bit kernel, 64-bit on a 64-bit one. That is not what the `libc` crate
/// provides - musl's 32-bit targets use a 64-bit `time_t`, so its `timeval` is
/// the wrong shape for the camera, and the kernel rejects it with `EINVAL`.
#[repr(C)]
#[derive(Clone, Copy)]
struct KernelTimeval {
    seconds: KernelTime,
    microseconds: KernelTime,
}

/// The kernel's `time_t` for socket timeouts.
#[cfg(target_pointer_width = "32")]
type KernelTime = i32;

/// The kernel's `time_t` for socket timeouts.
#[cfg(target_pointer_width = "64")]
type KernelTime = i64;

const _: () = assert!(size_of::<KernelTimeval>() == 2 * size_of::<usize>());
const _: () = assert!(size_of::<libc::sockaddr_un>() == SUN_PATH_LEN + 2);

/// An owned listening socket.
#[derive(Debug)]
pub struct UnixListener {
    fd: c_int,
}

impl UnixListener {
    /// Create, bind and start listening on `path`.
    pub fn bind(path: &str) -> Result<Self, Error> {
        let path = CString::new(path).map_err(|_| Error::from_errno(libc::EINVAL))?;
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(Error::last());
        }
        let listener = Self { fd };
        // A socket file left behind by a crash would fail `bind` with EADDRINUSE.
        unsafe { libc::unlink(path.as_ptr()) };
        bind_address(fd, &path)?;
        if unsafe { libc::chmod(path.as_ptr(), SOCKET_MODE) } < 0 {
            return Err(Error::last());
        }
        if unsafe { libc::listen(fd, BACKLOG) } < 0 {
            return Err(Error::last());
        }
        Ok(listener)
    }

    /// Accept one connection.
    pub fn accept(&self) -> Result<UnixStream, Error> {
        loop {
            let fd = unsafe {
                libc::accept4(
                    self.fd,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd >= 0 {
                return Ok(UnixStream { fd });
            }
            let error = Error::last();
            if !error.is_interrupted() {
                return Err(error);
            }
        }
    }

    /// The raw descriptor, used to register the socket with the shutdown state.
    pub fn raw_fd(&self) -> c_int {
        self.fd
    }
}

impl Drop for UnixListener {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// An owned accepted connection.
#[derive(Debug)]
pub struct UnixStream {
    fd: c_int,
}

impl UnixStream {
    /// Own a close-on-exec duplicate so a queued writer cannot retain a stale fd.
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        // SAFETY: fcntl duplicates this live descriptor; the result is newly owned.
        let fd = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(Error::last());
        }
        Ok(Self { fd })
    }

    /// Ask the kernel for a receive timeout.
    ///
    /// Best effort: the reference engine ignores the result too, and a read that
    /// never times out is still woken when the peer closes or the process is
    /// asked to stop.
    pub fn set_receive_timeout(&self, microseconds: i32) -> Result<(), Error> {
        let timeout = KernelTimeval {
            seconds: (microseconds / 1_000_000) as KernelTime,
            microseconds: (microseconds % 1_000_000) as KernelTime,
        };
        let result = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &timeout as *const KernelTimeval as *const c_void,
                size_of::<KernelTimeval>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(Error::last());
        }
        Ok(())
    }

    /// Read once, returning the number of bytes read (0 when the peer closed).
    pub fn read(&self, buffer: &mut [u8]) -> Result<usize, Error> {
        let count =
            unsafe { libc::read(self.fd, buffer.as_mut_ptr() as *mut c_void, buffer.len()) };
        if count < 0 {
            return Err(Error::last());
        }
        Ok(count as usize)
    }

    /// Write all of `buffer`.
    pub fn write_all(&self, buffer: &[u8]) -> Result<(), Error> {
        write_all(self.fd, buffer)
    }

    /// Wake a thread blocked on this socket.
    ///
    /// The shutdown handler calls this; it is async-signal-safe.
    pub fn shutdown(&self) {
        unsafe { libc::shutdown(self.fd, libc::SHUT_RDWR) };
    }

    /// The raw descriptor, used to register the socket with the shutdown state.
    pub fn raw_fd(&self) -> c_int {
        self.fd
    }
}

impl Drop for UnixStream {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Write all of `buffer` to a connected descriptor.
///
/// Separate from [`UnixStream`] because record producers hold the daemon's
/// descriptor rather than the stream that owns it.
pub(crate) fn write_all(descriptor: c_int, buffer: &[u8]) -> Result<(), Error> {
    write_all_vectored(descriptor, buffer, &[])
}

/// Send a record's header and payload together without concatenating them.
/// Callers must serialize producers for the entire operation, including retries.
pub(crate) fn write_all_vectored(
    descriptor: c_int,
    mut first: &[u8],
    mut second: &[u8],
) -> Result<(), Error> {
    let start = monotonic_ms()?;
    while !first.is_empty() || !second.is_empty() {
        let remaining = IPC_WRITE_TIMEOUT_MS.saturating_sub(monotonic_ms()?.saturating_sub(start));
        if remaining == 0 {
            return Err(Error::from_errno(libc::ETIMEDOUT));
        }
        let mut vectors = [
            libc::iovec {
                iov_base: first.as_ptr() as *mut c_void,
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_ptr() as *mut c_void,
                iov_len: second.len(),
            },
        ];
        // SAFETY: A zeroed msghdr represents no address or ancillary data.
        // Both iovecs point to live immutable slices for this synchronous send;
        // sendmsg does not modify their contents despite the C pointer types.
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = vectors.as_mut_ptr();
        message.msg_iovlen = vectors.len() as _;
        // SAFETY: The message and its two buffers remain valid until sendmsg
        // returns. MSG_NOSIGNAL turns a disconnected peer into EPIPE rather
        // than terminating the process, independent of signal installation.
        let written = unsafe {
            libc::sendmsg(
                descriptor,
                &message,
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if written < 0 {
            let error = Error::last();
            if error.is_interrupted() {
                continue;
            }
            if error.would_block() {
                let mut socket = libc::pollfd {
                    fd: descriptor,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: one live pollfd; the timeout is a bounded millisecond count.
                let status = unsafe { libc::poll(&mut socket, 1, remaining as libc::c_int) };
                if status == 0 {
                    return Err(Error::from_errno(libc::ETIMEDOUT));
                }
                if status < 0 && !Error::last().is_interrupted() {
                    return Err(Error::last());
                }
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(Error::from_errno(libc::EPIPE));
        }
        let written = written as usize;
        if written < first.len() {
            first = &first[written..];
        } else {
            second = &second[written - first.len()..];
            first = &[];
        }
    }
    Ok(())
}

/// Two-second private IPC write budget: a wedged daemon must not retain the
/// writer and vendor capture buffers indefinitely. Unlike SO_SNDTIMEO this is
/// one monotonic deadline across all partial sends and interruptions.
const IPC_WRITE_TIMEOUT_MS: u64 = 2_000;

fn monotonic_ms() -> Result<u64, Error> {
    // The legacy Linux syscall uses kernel word-sized timespec fields. Calling
    // musl's time64 binding against vendor uClibc would use the wrong ABI.
    #[repr(C)]
    struct KernelTimespec {
        seconds: KernelTime,
        nanoseconds: KernelTime,
    }
    let mut time = KernelTimespec {
        seconds: 0,
        nanoseconds: 0,
    };
    // SAFETY: CLOCK_MONOTONIC writes one correctly sized kernel timespec.
    let status = unsafe {
        libc::syscall(
            libc::SYS_clock_gettime,
            libc::CLOCK_MONOTONIC,
            &mut time as *mut KernelTimespec,
        )
    };
    if status < 0 {
        return Err(Error::last());
    }
    Ok((time.seconds as u64).saturating_mul(1_000) + time.nanoseconds as u64 / 1_000_000)
}

fn bind_address(fd: c_int, path: &CString) -> Result<(), Error> {
    let bytes = path.as_bytes();
    if bytes.len() >= SUN_PATH_LEN {
        return Err(Error::from_errno(libc::ENAMETOOLONG));
    }

    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length = (size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    let result = unsafe {
        libc::bind(
            fd,
            &address as *const libc::sockaddr_un as *const libc::sockaddr,
            length,
        )
    };
    if result < 0 {
        return Err(Error::last());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream as StdStream;

    fn socket_path(name: &str) -> alloc::string::String {
        alloc::format!(
            "/tmp/c220-media-engine-{}-{}.sock",
            std::process::id(),
            name
        )
    }

    #[test]
    fn accepts_a_connection_and_moves_bytes() {
        let path = socket_path("net");
        let listener = UnixListener::bind(&path).expect("listener should bind");
        // The engine links uClibc's accept path; on the host the same call runs
        // against the host libc, which is what makes this layer testable.
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = StdStream::connect(&path).expect("client should connect");
                stream.write_all(b"ping").expect("client should write");
                let mut received = [0_u8; 4];
                stream
                    .read_exact(&mut received)
                    .expect("client should read");
                received
            }
        });

        let accepted = listener.accept().expect("accept should succeed");
        let mut received = [0_u8; 4];
        let count = accepted.read(&mut received).expect("read should succeed");
        assert_eq!(&received[..count], b"ping");
        accepted.write_all(b"pong").expect("write should succeed");

        assert_eq!(client.join().expect("client thread"), *b"pong");
        std::fs::remove_file(&path).expect("socket file should be removable");
    }

    // The new scatter/gather writer must retry partial sends without losing
    // the header boundary, and a disconnected daemon must not raise SIGPIPE.
    #[test]
    fn vectored_writes_handle_large_payloads_and_disconnected_peers() {
        use std::os::fd::{AsRawFd, IntoRawFd};
        let (writer, mut reader) = StdStream::pair().expect("socket pair");
        let descriptor = writer.as_raw_fd();
        let buffer_size: c_int = 1_024;
        // SAFETY: descriptor is live; buffer_size is a correctly sized int
        // socket option. The small send buffer forces the record across sends.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    descriptor,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buffer_size as *const c_int as *const c_void,
                    size_of::<c_int>() as libc::socklen_t,
                )
            },
            0
        );
        let header = *b"C220-test-header";
        let payload = alloc::vec![0xab; 512 * 1024];
        let expected = [header.as_slice(), payload.as_slice()].concat();
        let receive = std::thread::spawn(move || {
            let mut bytes = alloc::vec![0; expected.len()];
            reader.read_exact(&mut bytes).expect("read complete record");
            assert_eq!(bytes, expected);
        });
        write_all_vectored(descriptor, &header, &payload).expect("send complete record");
        receive.join().expect("receiver thread");

        let (writer, reader) = StdStream::pair().expect("socket pair");
        let writer = UnixStream {
            fd: writer.into_raw_fd(),
        };
        drop(reader);
        assert_eq!(
            write_all_vectored(writer.raw_fd(), &header, &payload)
                .expect_err("disconnected peer")
                .errno(),
            Some(libc::EPIPE)
        );
    }

    #[test]
    fn rejects_a_path_longer_than_the_socket_field() {
        let path = alloc::format!("/tmp/{}", "x".repeat(SUN_PATH_LEN));
        assert_eq!(
            UnixListener::bind(&path)
                .expect_err("overlong path")
                .errno(),
            Some(libc::ENAMETOOLONG)
        );
    }

    #[test]
    fn sets_a_receive_timeout() {
        let path = socket_path("timeout");
        let listener = UnixListener::bind(&path).expect("listener should bind");
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let stream = StdStream::connect(&path).expect("client should connect");
                std::thread::sleep(std::time::Duration::from_millis(200));
                drop(stream);
            }
        });

        let accepted = listener.accept().expect("accept should succeed");
        accepted
            .set_receive_timeout(50_000)
            .expect("timeout should be accepted");
        let mut buffer = [0_u8; 8];
        let error = accepted
            .read(&mut buffer)
            .expect_err("read should time out");
        assert!(error.would_block(), "{error:?}");

        client.join().expect("client thread");
        std::fs::remove_file(&path).expect("socket file should be removable");
    }
}
