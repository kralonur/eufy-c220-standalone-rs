use super::*;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

// Opening TCP without a ClientHello must release the pre-authentication task.
#[tokio::test]
async fn stalled_client_hello_expires() {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(rustls::server::ResolvesServerCertUsingSni::new()));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let _client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        accept(TlsAcceptor::from(Arc::new(config)), stream),
    )
    .await
    .unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
}
