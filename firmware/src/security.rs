//! Camera-side security configuration loaded from environment/NVRAM.
//!
//! The boot script maps `/home/nvram/system/standalone_auth.conf` into these
//! environment names. Passwords and private keys remain on writable NVRAM and
//! are never part of the SquashFS image.

use crate::digest::DigestAuth;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Unix permission bits that grant a secret file access to group or other
/// users. Password files must keep all of these bits clear.
#[cfg(unix)]
const INSECURE_SECRET_MODE_BITS: u32 = 0o077;

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

fn read_secret_file(path: &str, variable: &str) -> io::Result<String> {
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
        if metadata.uid() != 0 || metadata.permissions().mode() & INSECURE_SECRET_MODE_BITS != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{variable} must be root-owned and mode 0600 or stricter: {}",
                    path.display()
                ),
            ));
        }
    }
    std::fs::read_to_string(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{variable} points to unreadable file {}: {error}",
                path.display()
            ),
        )
    })
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
}
