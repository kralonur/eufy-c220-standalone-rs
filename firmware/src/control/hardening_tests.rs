//! Black-box regressions for idle-client resource exhaustion and lost shutdown.
use super::*;
use tokio::net::TcpStream;

fn server() -> ControlServer {
    ControlServer::start(
        ControlServerConfig::new("127.0.0.1:0".parse().unwrap()),
        MediaEngineSlot::default(),
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn incomplete_requests_expire_without_holding_a_connection_forever() {
    let server = server();
    let mut client = TcpStream::connect(server.local_addr()).await.unwrap();
    client
        .write_all(b"GET /healthz HTTP/1.1\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 408"));
}

#[tokio::test]
async fn admission_rejects_excess_idle_clients_and_recovers_when_they_leave() {
    let server = server();
    let mut clients = Vec::new();
    for _ in 0..security::MAX_CONTROL_CONNECTIONS {
        clients.push(TcpStream::connect(server.local_addr()).await.unwrap());
    }
    tokio::task::yield_now().await;
    let mut excess = TcpStream::connect(server.local_addr()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), excess.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(
        response.is_empty(),
        "excess clients must be closed before parsing input"
    );
    drop(clients);
    tokio::task::yield_now().await;
    let mut client = TcpStream::connect(server.local_addr()).await.unwrap();
    client
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    // A missing media engine is legitimate; accepting and dispatching the
    // request, rather than retaining the spent permits, is the regression.
    assert!(response.starts_with(b"HTTP/1.1 503"));
}

#[tokio::test]
async fn dropping_before_first_poll_releases_the_listener_and_speaker_worker() {
    let server = server();
    let address = server.local_addr();
    let inner = Arc::downgrade(&server.inner);
    drop(server);
    tokio::time::timeout(Duration::from_secs(1), async {
        while inner.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(StdTcpListener::bind(address).is_ok());
}
