use super::*;

// Startup is transactional: a TLS bind failure must release the already-bound
// plaintext socket instead of leaving an unreachable background accept task.
#[tokio::test]
async fn failed_tls_startup_releases_plaintext_listener() {
    let plaintext = StdTcpListener::bind(("127.0.0.1", 0)).expect("reserve plaintext address");
    let address = plaintext.local_addr().expect("plaintext address");
    drop(plaintext);
    let occupied = StdTcpListener::bind(("0.0.0.0", 0)).expect("occupy TLS port");
    let tls_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("TLS versions")
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(rustls::server::ResolvesServerCertUsingSni::new()));
    let mut config = RtspServerConfig::new(address);
    config.rtsps = Some(RtspsConfig {
        port: occupied.local_addr().expect("TLS address").port(),
        acceptor: TlsAcceptor::from(Arc::new(tls_config)),
    });
    assert_eq!(
        RtspServer::start(config)
            .expect_err("TLS port already bound")
            .kind(),
        io::ErrorKind::AddrInUse
    );
    StdTcpListener::bind(address).expect("failed startup must release plaintext port");
}

// A stop request made before the accept task's first poll must be retained.
#[tokio::test]
async fn dropping_server_before_accept_task_runs_releases_listener() {
    let server = RtspServer::start(RtspServerConfig::new(
        "127.0.0.1:0".parse().expect("address"),
    ))
    .expect("start server");
    let address = server.local_addr();
    let clone = server.clone();
    drop(server);
    tokio::task::yield_now().await;
    assert!(
        StdTcpListener::bind(address).is_err(),
        "clone keeps listener alive"
    );
    drop(clone);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if StdTcpListener::bind(address).is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("last handle releases listener");

    let server = RtspServer::start(RtspServerConfig::new(
        "127.0.0.1:0".parse().expect("address"),
    ))
    .expect("start server without polling its tasks");
    let address = server.local_addr();
    drop(server);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if StdTcpListener::bind(address).is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pre-poll stop must be retained");
}

// Unauthenticated idle clients must not retain unbounded tasks or admission.
#[tokio::test]
async fn idle_setup_expires_and_admission_recovers() {
    let server = RtspServer::start(RtspServerConfig::new("127.0.0.1:0".parse().unwrap())).unwrap();
    let mut client = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let mut clients = Vec::new();
    for _ in 0..crate::security::MAX_RTSP_CONNECTIONS {
        clients.push(
            tokio::net::TcpStream::connect(server.local_addr())
                .await
                .unwrap(),
        );
    }
    tokio::task::yield_now().await;
    let mut excess = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(SETUP_DEADLINE / 2, excess.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    drop(clients);
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.inner.connections.available_permits() != crate::security::MAX_RTSP_CONNECTIONS
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
