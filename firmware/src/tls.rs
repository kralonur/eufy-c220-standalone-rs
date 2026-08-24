//! TLS acceptor construction shared by the control API and RTSPS.
//!
//! Certificate and private-key paths are supplied from writable camera NVRAM;
//! no key material is embedded in the firmware image.

use rustls_pki_types::pem::PemObject;
use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Unix permission bits that grant a private key to group or other users.
#[cfg(unix)]
const INSECURE_KEY_MODE_BITS: u32 = 0o077;
use tokio_rustls::TlsAcceptor;

/// Build a TLS server acceptor from a PEM certificate chain and private key.
///
/// The caller decides whether an unreadable pair disables a listener or causes
/// startup to fail. This function never silently substitutes a plaintext
/// connection for a configured TLS listener.
pub fn build_acceptor(cert_file: &Path, key_file: &Path) -> io::Result<TlsAcceptor> {
    if !cert_file.is_absolute() || !key_file.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TLS certificate and key paths must be absolute",
        ));
    }
    let certificates: Vec<rustls_pki_types::CertificateDer<'static>> =
        rustls_pki_types::CertificateDer::pem_file_iter(cert_file)
            .map_err(|error| {
                io::Error::other(format!(
                    "certificate {} is unreadable: {error}",
                    cert_file.display()
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                io::Error::other(format!(
                    "certificate {} is not valid PEM: {error}",
                    cert_file.display()
                ))
            })?;
    if certificates.is_empty() {
        return Err(io::Error::other(format!(
            "certificate {} contains no certificate",
            cert_file.display()
        )));
    }

    let key_metadata = std::fs::metadata(key_file).map_err(|error| {
        io::Error::other(format!("key {} is unreadable: {error}", key_file.display()))
    })?;
    if !key_metadata.is_file() {
        return Err(io::Error::other(format!(
            "key {} is not a regular file",
            key_file.display()
        )));
    }
    #[cfg(unix)]
    if key_metadata.uid() != 0 || key_metadata.permissions().mode() & INSECURE_KEY_MODE_BITS != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "key {} must be root-owned and mode 0600 or stricter",
                key_file.display()
            ),
        ));
    }
    let key = rustls_pki_types::PrivateKeyDer::from_pem_file(key_file).map_err(|error| {
        io::Error::other(format!("key {} is unreadable: {error}", key_file.display()))
    })?;
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|error| io::Error::other(format!("TLS 1.3 configuration was rejected: {error}")))?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .map_err(|error| io::Error::other(format!("certificate and key were rejected: {error}")))?;
    Ok(TlsAcceptor::from(std::sync::Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_certificate_is_an_error() {
        let error = match build_acceptor(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        ) {
            Ok(_) => panic!("missing certificate must not build an acceptor"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("/nonexistent/cert.pem"));
    }

    #[test]
    fn non_pem_certificate_is_an_error() {
        let directory = std::env::temp_dir().join(format!(
            "c220-tls-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("test clock must be after the Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("test directory should be created");
        let cert = directory.join("cert.pem");
        let key = directory.join("key.pem");
        std::fs::write(&cert, "not a certificate\n").expect("test certificate should be written");
        std::fs::write(&key, "not a key\n").expect("test key should be written");
        assert!(build_acceptor(&cert, &key).is_err());
        std::fs::remove_dir_all(directory).ok();
    }
}
