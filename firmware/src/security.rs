//! Camera-side security configuration loaded from environment/NVRAM.
//!
//! The boot script maps `/home/nvram/system/standalone_auth.conf` into these
//! environment names. Passwords and private keys remain on writable NVRAM and
//! are never part of the SquashFS image.

use crate::digest::DigestAuth;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Unix permission bits that grant a secret file access to group or other
/// users. Password files must keep all of these bits clear.
#[cfg(unix)]
const INSECURE_SECRET_MODE_BITS: u32 = 0o077;

/// Deployment admission budget, including unauthenticated connections and TLS
/// handshakes: eight concurrent API requests leave room for polling and controls.
pub(crate) const MAX_CONTROL_CONNECTIONS: usize = 8;

/// Shared RTSP/RTSPS admission budget, including pre-authentication and TLS.
pub(crate) const MAX_RTSP_CONNECTIONS: usize = 4;

/// Longest DNS label, from RFC 1035.
const MAX_HOST_LABEL_LENGTH: usize = 63;

/// Load one Digest credential pair.
///
/// Both variables must be present to enable authentication. A partial pair is
/// rejected so a typo cannot silently turn one protected listener anonymous.
/// Production startup requires the returned value to be `Some`; `None` exists
/// only for the explicit board bring-up mode.
pub fn load_digest_auth(
    username_variable: &str,
    password_file_variable: &str,
) -> io::Result<Option<Arc<DigestAuth>>> {
    let username = env_string(username_variable)?;
    let password_file = env_string(password_file_variable)?;
    match (username, password_file) {
        (None, None) => Ok(None),
        (Some(username), Some(password_file)) => {
            if username.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{username_variable} must not be empty"),
                ));
            }
            let password = read_secret_file(&password_file, password_file_variable)?;
            let password = password.trim();
            if password.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("password file {password_file} is empty"),
                ));
            }
            DigestAuth::new(username.trim(), password)
                .map(Arc::new)
                .map(Some)
        }
        (Some(_), None) | (None, Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{username_variable} and {password_file_variable} must be configured together"),
        )),
    }
}

fn env_string(variable: &str) -> io::Result<Option<String>> {
    match std::env::var(variable) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{variable} is not valid UTF-8"),
        )),
    }
}

/// Read a secret from the file named by `variable`, applying the same ownership and
/// mode checks the service credentials get. Returns `None` when unset.
pub fn secret_file(variable: &str) -> io::Result<Option<Zeroizing<String>>> {
    match env_string(variable)? {
        Some(path) => read_secret_file(&path, variable).map(Some),
        None => Ok(None),
    }
}

fn read_secret_file(path: &str, variable: &str) -> io::Result<Zeroizing<String>> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{variable} must use an absolute path: {}", path.display()),
        ));
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{variable} points to unreadable file {}: {error}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{variable} must point to a regular file: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        // SAFETY: geteuid takes no arguments and cannot fail.
        let ours = unsafe { libc::geteuid() };
        if (metadata.uid() != 0 && metadata.uid() != ours)
            || metadata.permissions().mode() & INSECURE_SECRET_MODE_BITS != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{variable} must be owned by root or the service user and mode 0600 or stricter: {}",
                    path.display()
                ),
            ));
        }
    }
    std::fs::read_to_string(path)
        .map(Zeroizing::new)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{variable} points to unreadable file {}: {error}",
                    path.display()
                ),
            )
        })
}

/// Read the camera's own name: the mDNS host name and the first name in a
/// generated certificate.
///
/// Only a DNS label is accepted. The value ends up in the subject alternative
/// names of a certificate, where a name that cannot be resolved or verified is
/// worse than no name at all, so anything that is not a plain label is refused
/// rather than passed through. A trailing `.local` is accepted and ignored, so
/// `c220` and `c220.local` mean the same camera to whoever writes the config.
pub fn host_label(variable: &str, default: &str) -> io::Result<String> {
    let raw = match std::env::var(variable) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(default.to_string()),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{variable} is not valid UTF-8"),
            ));
        }
    };
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Ok(default.to_string());
    }
    let label = value.strip_suffix(".local").unwrap_or(&value);
    if !is_host_label(label) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{variable} must be a DNS name such as c220 or c220.local, not {raw:?}"),
        ));
    }
    Ok(label.to_string())
}

/// Whether the value is a single DNS label (RFC 1035): lowercase letters, digits
/// and inner hyphens, no longer than 63 characters.
fn is_host_label(label: &str) -> bool {
    if label.is_empty() || label.len() > MAX_HOST_LABEL_LENGTH {
        return false;
    }
    if label.starts_with('-') || label.ends_with('-') {
        return false;
    }
    label
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Read a port override, using the supplied default when unset. Port 0 is
/// accepted because it explicitly disables a listener.
pub fn port(variable: &str, default: u16) -> io::Result<u16> {
    match std::env::var(variable) {
        Ok(value) => value.trim().parse::<u16>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{variable} must be a port number: {error}"),
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(io::Error::other(format!(
            "could not read {variable}: {error}"
        ))),
    }
}

/// Parse a conventional boolean environment override.
pub fn flag(variable: &str, default: bool) -> io::Result<bool> {
    match std::env::var(variable) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{variable} must be true or false"),
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(io::Error::other(format!(
            "could not read {variable}: {error}"
        ))),
    }
}

/// Return a non-empty path environment override.
pub fn path(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn unset_credentials_leave_auth_disabled() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Tests serialize environment mutation; no other test can observe these
        // temporary values while the lock is held.
        unsafe {
            std::env::remove_var("C220_TEST_USER");
            std::env::remove_var("C220_TEST_PASSWORD_FILE");
        }
        assert!(
            load_digest_auth("C220_TEST_USER", "C220_TEST_PASSWORD_FILE")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn partial_credentials_are_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Tests serialize environment mutation; no other test can observe these
        // temporary values while the lock is held.
        unsafe {
            std::env::set_var("C220_TEST_USER", "camera");
            std::env::remove_var("C220_TEST_PASSWORD_FILE");
        }
        let error = match load_digest_auth("C220_TEST_USER", "C220_TEST_PASSWORD_FILE") {
            Ok(_) => panic!("partial credentials must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("configured together"));
        // The same lock protects cleanup from concurrent environment readers.
        unsafe {
            std::env::remove_var("C220_TEST_USER");
        }
    }

    /// A bad name here would end up inside a certificate's subject alternative
    /// names, where it cannot be verified and cannot be fixed without reissuing.
    #[test]
    fn host_labels_are_validated_before_they_reach_a_certificate() {
        let _guard = ENV_LOCK.lock().unwrap();
        let variable = "C220_TEST_HOST_NAME";
        let cases = [
            ("c220", Some("c220")),
            ("c220.local", Some("c220")),
            ("  Front-Door.Local  ", Some("front-door")),
            ("front-door-2", Some("front-door-2")),
            ("", Some("c220")),
            ("-camera", None),
            ("camera-", None),
            ("camera_box", None),
            ("camera.example.com", None),
            ("camera!", None),
            (&"a".repeat(64), None),
        ];
        for (input, expected) in cases {
            // Tests serialize environment mutation; no other test can observe
            // these temporary values while the lock is held.
            unsafe { std::env::set_var(variable, input) };
            let result = host_label(variable, "c220").ok();
            assert_eq!(
                result.as_deref(),
                expected,
                "input {input:?} should be {expected:?}"
            );
        }
        unsafe { std::env::remove_var(variable) };
        assert_eq!(
            host_label(variable, "c220").expect("an unset name uses the default"),
            "c220"
        );
    }
}
