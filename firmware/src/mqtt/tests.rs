use super::*;

// Missing CA configuration must fail before credentials or commands can be
// transmitted; it must never select the old plaintext transport.
#[test]
fn broker_configuration_requires_an_absolute_ca_path() {
    assert_eq!(
        require_ca_file(None).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        require_ca_file(Some("relative.pem".into()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let path = PathBuf::from("/tmp/c220-public-ca.pem");
    assert_eq!(require_ca_file(Some(path.clone())).unwrap(), path);
}
