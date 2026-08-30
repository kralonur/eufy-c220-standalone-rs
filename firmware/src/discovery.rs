//! mDNS advertisement, so the camera can be reached by name instead of by address.
//!
//! The camera answers for one name on the local network and advertises its stream
//! as an RTSP service. This is what makes a generated certificate useful without an
//! address book: the certificate names the same host, so a client that dials the
//! name verifies it, and nothing has to be reissued when DHCP moves the camera.
//! It is also how a second camera is distinguished - one name each - which is why
//! the name is configuration rather than a constant.

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::io;

/// The registered service type for RTSP.
const SERVICE_TYPE: &str = "_rtsp._tcp.local.";

/// Path of the video stream inside the RTSP session, published so a client that
/// only browses the network has something to work with.
const STREAM_PATH: &str = "/live";

/// TXT key stating that this service is encrypted. The camera speaks TLS 1.3 on
/// this port and plaintext RTSP is retired, so a client that reads only the TXT
/// records is told rather than left to discover it.
const TLS_KEY: &str = "tls";

/// TXT key naming the stream path.
const PATH_KEY: &str = "path";

/// A live advertisement.
///
/// Keeping this value alive keeps the service registered; dropping it withdraws
/// the service. The firmware holds it for as long as it runs.
pub struct Advertisement {
    // Held so the daemon outlives the registration call. The field is never read.
    _daemon: ServiceDaemon,
}

/// Advertise `host_label`.local and the RTSP service on `port`.
///
/// `addresses` are the addresses the camera holds now. They are passed in rather
/// than left to be discovered, because this firmware starts before Wi-Fi has one:
/// registering with no addresses leaves the camera unknown until the responder
/// refreshes them, which took minutes in testing. Address tracking stays enabled,
/// so later changes are still followed.
///
/// Errors are reported to the caller rather than raised as a failure to serve: a
/// camera that cannot advertise is still a working camera, and discovery is not
/// worth refusing a connection over.
pub fn advertise(host_label: &str, port: u16, addresses: &[String]) -> io::Result<Advertisement> {
    let daemon = ServiceDaemon::new()
        .map_err(|error| io::Error::other(format!("mDNS responder could not start: {error}")))?;

    // Address tracking stays on so the advertised address follows the camera onto
    // the rescue network and through a DHCP change.
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        host_label,
        &format!("{host_label}.local."),
        addresses,
        port,
        &[(TLS_KEY, "1"), (PATH_KEY, STREAM_PATH)][..],
    )
    .map_err(|error| io::Error::other(format!("mDNS service was rejected: {error}")))?
    .enable_addr_auto();

    daemon
        .register(service)
        .map_err(|error| io::Error::other(format!("mDNS registration failed: {error}")))?;
    Ok(Advertisement { _daemon: daemon })
}
