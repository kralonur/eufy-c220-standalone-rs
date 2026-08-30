//! First-boot self-signed certificate generation.
//!
//! The camera ships no PKI tooling (this rootfs has no openssl) and a new owner
//! should not need a certificate authority on a workstation before the camera
//! can serve anything, so the firmware creates its own certificate the first
//! time it needs one. Encryption stays mandatory; only the ceremony is removed.
//!
//! A certificate the owner installed is never touched: generation happens only
//! when neither file exists, and a half-present pair is refused rather than
//! replaced, because the missing half may be the one the owner is about to copy
//! in.
//!
//! Pinning is the intended client behaviour, so an existing certificate is
//! deliberately stable: it is generated once and then left alone, even if the
//! camera's address later changes. Supplying a proper certificate, or deleting
//! both files and rebooting to get a new one, are the supported ways to change it.

use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Suffix the camera's name is published under. It is the reserved mDNS domain,
/// so a client resolves it by asking the local network rather than a server.
const LOCAL_SUFFIX: &str = ".local";

/// Subject fields of the generated certificate, so a client that shows the
/// issuer says something meaningful instead of a library placeholder.
const SUBJECT_ORGANIZATION: &str = "eufy-c220-standalone";
const SUBJECT_COMMON_NAME: &str = "c220 camera";

/// Loopback is included so tools running on the camera itself can verify TLS.
const LOOPBACK: &str = "127.0.0.1";

/// Longest a first boot waits for a usable address before falling back to the
/// host name alone. The address is what the owner types into their client, so it
/// is worth a short wait: Wi-Fi is still associating when the firmware starts.
const ADDRESS_WAIT: Duration = Duration::from_secs(30);

/// How often the address is re-checked while waiting.
const ADDRESS_POLL: Duration = Duration::from_secs(1);

/// A private key is a secret; a certificate is public.
const KEY_MODE: u32 = 0o600;
const CERT_MODE: u32 = 0o644;

/// What [`ensure_self_signed`] found or did.
pub enum Outcome {
    /// Both files already existed and were left untouched.
    Existing,
    /// A new pair was written, valid for these subject alternative names.
    Generated(Vec<String>),
}

/// Make sure a usable certificate and key exist at the configured paths.
///
/// `host_label` is the camera's own name, used for the first subject alternative
/// name so the certificate matches the mDNS name the camera answers for.
///
/// Returns [`Outcome::Existing`] without reading or writing anything when both
/// files are already present.
pub fn ensure_self_signed(
    cert_file: &Path,
    key_file: &Path,
    host_label: &str,
) -> io::Result<Outcome> {
    if !cert_file.is_absolute() || !key_file.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "certificate and key paths must be absolute",
        ));
    }
    match (cert_file.exists(), key_file.exists()) {
        (true, true) => return Ok(Outcome::Existing),
        (true, false) | (false, true) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "only one of {} and {} exists; refusing to replace a half-present pair",
                    cert_file.display(),
                    key_file.display()
                ),
            ));
        }
        (false, false) => {}
    }

    let names = names_when_addressed(host_label);
    generate_and_write(cert_file, key_file, &names)?;
    Ok(Outcome::Generated(names))
}

/// Subject alternative names for a camera at its current address.
///
/// The host name comes first so it is always present even when the camera has no
/// address yet, which is the case on a first boot that has not associated.
fn names_when_addressed(host_label: &str) -> Vec<String> {
    let deadline = Instant::now() + ADDRESS_WAIT;
    loop {
        let names = subject_names(host_label);
        if names.len() > 2 || Instant::now() >= deadline {
            return names;
        }
        std::thread::sleep(ADDRESS_POLL);
    }
}

/// The camera's name, loopback and every current non-loopback IPv4 address.
fn subject_names(host_label: &str) -> Vec<String> {
    let mut names = vec![format!("{host_label}{LOCAL_SUFFIX}"), LOOPBACK.to_string()];
    for address in local_addresses() {
        let text = address.to_string();
        if !names.contains(&text) {
            names.push(text);
        }
    }
    names
}

/// Enumerate this host's non-loopback IPv4 interface addresses.
///
/// Shared with discovery, which has the same problem as certificate generation:
/// the firmware starts before Wi-Fi has an address, and both need one.
pub fn local_addresses() -> Vec<Ipv4Addr> {
    let mut addresses = Vec::new();
    // SAFETY: getifaddrs fills a list we own and must release with freeifaddrs.
    // Each entry is dereferenced only after a null check, and the address is read
    // as sockaddr_in only when the entry announces AF_INET.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return addresses;
        }
        let mut entry = head;
        while !entry.is_null() {
            let current = &*entry;
            if !current.ifa_addr.is_null() && (*current.ifa_addr).sa_family as i32 == libc::AF_INET
            {
                let socket = &*(current.ifa_addr as *const libc::sockaddr_in);
                // s_addr is in network byte order, so its bytes in memory are
                // already the four octets in order.
                let address = Ipv4Addr::from(socket.sin_addr.s_addr.to_ne_bytes());
                if !address.is_loopback() && !address.is_unspecified() {
                    addresses.push(address);
                }
            }
            entry = current.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    addresses
}

/// Generate a self-signed pair and install it at the configured paths.
fn generate_and_write(cert_file: &Path, key_file: &Path, names: &[String]) -> io::Result<()> {
    let mut parameters = rcgen::CertificateParams::new(names.to_vec())
        .map_err(|error| io::Error::other(format!("certificate names were rejected: {error}")))?;
    parameters
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, SUBJECT_ORGANIZATION);
    parameters
        .distinguished_name
        .push(rcgen::DnType::CommonName, SUBJECT_COMMON_NAME);
    // The default validity runs from 1975 to 4096, which matters here: this board
    // has no real-time clock and can boot with a stale time.
    let signing_key = rcgen::KeyPair::generate()
        .map_err(|error| io::Error::other(format!("key generation failed: {error}")))?;
    let certificate = parameters
        .self_signed(&signing_key)
        .map_err(|error| io::Error::other(format!("signing failed: {error}")))?;
    // The key is written first: if this fails part way, the next boot reports the
    // half-present pair instead of starting a listener with a certificate whose
    // key does not exist.
    write_atomic(key_file, &signing_key.serialize_pem(), KEY_MODE)?;
    match write_atomic(cert_file, &certificate.pem(), CERT_MODE) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(key_file);
            Err(error)
        }
    }
}

/// Write a file that must never be visible in a less restrictive state.
///
/// The mode is applied as the file is created, so a private key is never briefly
/// readable, and the content appears at its final path only once complete.
fn write_atomic(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a file path", path.display()),
            )
        })?
        .to_string_lossy()
        .into_owned();
    let temporary = parent.join(format!(".{file_name}.tmp"));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(mode);

    let result = (|| -> io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A directory that only this test uses, removed when the guard drops.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "c220-identity-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("test clock must be after the Unix epoch")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&directory).expect("scratch directory should be created");
            Self(directory)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Overwriting an installed certificate would rotate a pinned identity and
    /// could replace a properly signed certificate with a self-signed one.
    #[test]
    fn an_existing_pair_is_left_untouched() {
        let scratch = Scratch::new("existing");
        let cert = scratch.0.join("cert.pem");
        let key = scratch.0.join("key.pem");
        std::fs::write(&cert, "installed certificate\n").expect("certificate should be written");
        std::fs::write(&key, "installed key\n").expect("key should be written");

        let outcome =
            ensure_self_signed(&cert, &key, "c220").expect("an existing pair must be accepted");

        assert!(matches!(outcome, Outcome::Existing));
        assert_eq!(
            std::fs::read_to_string(&cert).expect("certificate should be readable"),
            "installed certificate\n"
        );
        assert_eq!(
            std::fs::read_to_string(&key).expect("key should be readable"),
            "installed key\n"
        );
    }

    /// The generated pair has to be usable by the TLS listener, and the private
    /// key must never be group or world readable.
    #[test]
    fn a_generated_pair_parses_and_keeps_the_key_private() {
        let scratch = Scratch::new("generated");
        let cert = scratch.0.join("cert.pem");
        let key = scratch.0.join("key.pem");
        let names = vec!["front-door.local".to_string(), "192.168.1.55".to_string()];

        generate_and_write(&cert, &key, &names).expect("generation should succeed");

        use rustls_pki_types::pem::PemObject;
        let certificates: Vec<_> = rustls_pki_types::CertificateDer::pem_file_iter(&cert)
            .expect("certificate should be readable PEM")
            .collect::<Result<Vec<_>, _>>()
            .expect("certificate should parse");
        assert_eq!(certificates.len(), 1);
        rustls_pki_types::PrivateKeyDer::from_pem_file(&key)
            .expect("generated key should be a usable private key");

        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&key)
                .expect("key metadata should be readable")
                .permissions()
                .mode()
                & 0o777,
            KEY_MODE
        );
    }

    /// A half-present pair is ambiguous: generating would overwrite whichever
    /// half the owner placed, and starting would fail. It must be reported.
    #[test]
    fn a_half_present_pair_is_refused() {
        let scratch = Scratch::new("half");
        let cert = scratch.0.join("cert.pem");
        let key = scratch.0.join("key.pem");
        std::fs::write(&cert, "installed certificate\n").expect("certificate should be written");

        let error = ensure_self_signed(&cert, &key, "c220")
            .err()
            .expect("a half-present pair must be refused");

        assert!(error.to_string().contains("half-present"));
        assert!(
            !key.exists(),
            "the missing key must not be created for a half-present pair"
        );
    }
}
