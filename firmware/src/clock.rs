//! Best-effort SNTP clock sync for a board that has no RTC.
//!
//! The camera boots with a clock restored from NVRAM, which is plausible by
//! construction and can be arbitrarily stale after a power cut. Digest nonces
//! and TLS do not care - both use this board's clock consistently, and clients
//! validate the certificate against their own - but every log line carries the
//! timestamp, so a stale clock makes the logs lie.
//!
//! Only LAN servers are used: the operator's explicit choice and the default
//! gateway. A camera with a working route has something on the network that can
//! tell it the time; reaching for a public pool would add an external dependency
//! this firmware deliberately does not have.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds between 1900-01-01 (NTP epoch) and 1970-01-01 (Unix epoch).
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;
const NTP_PORT: u16 = 123;
/// LI = 0 (no warning), VN = 4 (SNTPv4), Mode = 3 (client).
const CLIENT_REQUEST: [u8; 48] = {
    let mut request = [0_u8; 48];
    request[0] = 0x1b;
    request
};
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_INTERVAL: Duration = Duration::from_secs(10);
/// Long enough for a slow DHCP/Wi-Fi bring-up, short enough that a
/// misconfigured network is reported instead of being retried forever.
const MAX_ATTEMPTS: u32 = 30;
/// Anything before this is a board that has never learned the date.
const PLAUSIBLE_EPOCH: u64 = 1_577_836_800; // 2020-01-01
/// How wrong the clock may be before a server answer replaces it. The boot
/// script restores the last known time, and that value can be hours stale while
/// looking perfectly plausible, so a set clock is not necessarily a right one.
const ADOPT_THRESHOLD_SECS: u64 = 30;
/// Attempts when the clock already looks set: enough to survive a slow Wi-Fi
/// bring-up without querying the LAN for five minutes over a few seconds.
const MAX_ATTEMPTS_WHEN_SET: u32 = 5;

const LAST_KNOWN_TIME_FILE: &str = "/home/nvram/system/.last_known_time";
/// Staging copy for the atomic rename. It must live on the *same* filesystem as
/// the target: `/tmp` is tmpfs and NVRAM is jffs2, and `rename(2)` across mounts
/// fails with EXDEV.
const STAGING_FILE: &str = "/home/nvram/system/.last_known_time.tmp";
/// Write at most once every 12 hours to avoid needless flash wear.
const MIN_PERSIST_INTERVAL_SECS: u64 = 12 * 3600;

/// Seconds since the Unix epoch, or `None` when the clock is before it.
fn current_seconds() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

/// Whether an answer differs enough from the current clock to be adopted.
fn should_adopt(current_secs: u64, server_secs: u64) -> bool {
    current_secs.abs_diff(server_secs) > ADOPT_THRESHOLD_SECS
}

/// The default route's gateway out of `/proc/net/route`.
fn default_gateway() -> Option<String> {
    std::fs::read_to_string("/proc/net/route")
        .ok()
        .and_then(|routes| gateway_from(&routes))
}

fn gateway_from(routes: &str) -> Option<String> {
    for line in routes.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let _interface = fields.next()?;
        let destination = fields.next()?;
        let gateway = fields.next()?;
        // The default route has destination 0.0.0.0.
        if destination != "00000000" {
            continue;
        }
        let raw = u32::from_str_radix(gateway, 16).ok()?;
        let octets = raw.to_le_bytes();
        return Some(format!(
            "{}.{}.{}.{}",
            octets[0], octets[1], octets[2], octets[3]
        ));
    }
    None
}

/// Servers to try, in order, without duplicates.
fn candidates(explicit: Option<&str>) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    let mut push = |value: Option<&str>| {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty())
            && !servers.iter().any(|known| known == value)
        {
            servers.push(value.to_string());
        }
    };
    push(explicit);
    push(default_gateway().as_deref());
    servers
}

/// Ask one server for the time. Returns `None` for anything unusable: a short
/// reply, a kiss-of-death (stratum 0), or a server repeating our own request.
async fn query(server: &str) -> Option<SystemTime> {
    let target: SocketAddr = format!("{server}:{NTP_PORT}").parse().ok()?;
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
    socket.send_to(&CLIENT_REQUEST, target).await.ok()?;

    let mut reply = [0_u8; 48];
    let len = tokio::time::timeout(QUERY_TIMEOUT, socket.recv(&mut reply))
        .await
        .ok()?
        .ok()?;
    parse_reply(&reply[..len])
}

fn parse_reply(reply: &[u8]) -> Option<SystemTime> {
    if reply.len() < 48 {
        return None;
    }
    let mode = reply[0] & 0x07;
    let stratum = reply[1];
    // Mode 4 is a server reply, 5 a broadcast; stratum 0 means "do not use me".
    if !matches!(mode, 4 | 5) || stratum == 0 {
        return None;
    }
    let seconds = u32::from_be_bytes(reply[40..44].try_into().ok()?) as u64;
    if seconds <= NTP_EPOCH_OFFSET {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(seconds - NTP_EPOCH_OFFSET))
}

/// Step the system clock. Only root can do this, and this service runs as root.
///
/// `libc::time_t` is deprecated only because its width changes with musl 1.2;
/// naming it is still the correct way to match the platform ABI, and `as _`
/// would hide the field type instead.
#[allow(deprecated)]
fn set_system_time(when: SystemTime) -> bool {
    let Ok(since_epoch) = when.duration_since(UNIX_EPOCH) else {
        return false;
    };
    let tv = libc::timespec {
        tv_sec: since_epoch.as_secs() as libc::time_t,
        tv_nsec: since_epoch.subsec_nanos() as libc::c_long,
    };
    // SAFETY: `tv` is a valid timespec built from a checked duration, and
    // CLOCK_REALTIME with a null result pointer is the documented call.
    unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &tv) == 0 }
}

/// Persist the last known timestamp to NVRAM so the boot script does not restore
/// an ancient value after a power cycle. Rate-limited and written atomically to
/// protect the SPI NOR flash.
fn persist_last_known_time() {
    let Some(now_secs) = current_seconds() else {
        return;
    };
    if now_secs < PLAUSIBLE_EPOCH {
        return;
    }
    if let Ok(existing) = std::fs::read_to_string(LAST_KNOWN_TIME_FILE)
        && let Ok(stored_secs) = existing.trim().parse::<u64>()
        && now_secs >= stored_secs
        && now_secs - stored_secs < MIN_PERSIST_INTERVAL_SECS
    {
        return;
    }
    if !std::path::Path::new("/home/nvram/system").is_dir() {
        return;
    }
    if std::fs::write(STAGING_FILE, format!("{now_secs}\n")).is_ok() {
        let _ = std::fs::rename(STAGING_FILE, LAST_KNOWN_TIME_FILE);
    }
}

/// Sync the clock once, retrying while the network comes up.
///
/// Candidates are rebuilt on every attempt: the default route, and with it the
/// gateway, only exists once the Wi-Fi interface is up, which is exactly when
/// the first attempts are running.
pub async fn sync_until_set(explicit: Option<String>) {
    let unset = current_seconds().is_none_or(|seconds| seconds < PLAUSIBLE_EPOCH);
    let attempts = if unset {
        MAX_ATTEMPTS
    } else {
        MAX_ATTEMPTS_WHEN_SET
    };
    let mut announced = false;
    let mut servers: Vec<String> = Vec::new();
    for attempt in 1..=attempts {
        servers = candidates(explicit.as_deref());
        if !servers.is_empty() && !announced {
            tracing::info!(?servers, unset, "asking LAN time servers for the time");
            announced = true;
        }
        for server in &servers {
            let Some(when) = query(server).await else {
                continue;
            };
            let server_secs = when
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default();
            let adopt = match current_seconds() {
                Some(current) => unset || should_adopt(current, server_secs),
                None => true,
            };
            if !adopt {
                tracing::info!(server = %server, "system clock already matches the LAN time server");
            } else if set_system_time(when) {
                tracing::info!(server = %server, "system clock set from NTP");
            } else {
                tracing::warn!(server = %server, "could not set the system clock");
            }
            // Persist even when the clock was already close: the stored value is
            // what the next boot restores, and it is rate-limited anyway.
            persist_last_known_time();
            return;
        }
        if attempt < attempts {
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }
    if unset {
        tracing::warn!(
            ?servers,
            "could not set the system clock; log timestamps will be wrong until a LAN time server answers"
        );
    } else {
        tracing::warn!(
            ?servers,
            "no LAN time server answered; keeping the restored clock, which may be stale"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_clock_is_adopted_and_a_close_one_is_not() {
        let now = 1_800_000_000;
        assert!(
            !should_adopt(now, now),
            "the same second is not a reason to set"
        );
        assert!(!should_adopt(now, now + 29));
        assert!(!should_adopt(now, now - 29));
        assert!(should_adopt(now, now + 31));
        // The case that matters: the boot script restored a time hours stale.
        assert!(should_adopt(now, now + 13 * 3600));
        // A server that is behind must be believed too, not only one ahead.
        assert!(should_adopt(now, now - 3600));
    }

    /// A server reply carrying the given Unix time.
    fn reply_for(unix_seconds: u64) -> Vec<u8> {
        let mut reply = vec![0_u8; 48];
        reply[0] = 0x24; // LI = 0, VN = 4, Mode = 4 (server)
        reply[1] = 2; // stratum
        reply[40..44].copy_from_slice(&((unix_seconds + NTP_EPOCH_OFFSET) as u32).to_be_bytes());
        reply
    }

    #[test]
    fn reads_the_transmit_timestamp() {
        let parsed = parse_reply(&reply_for(1_609_459_200)).expect("a valid reply");
        assert_eq!(
            parsed.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_609_459_200
        );
    }

    #[test]
    fn rejects_unusable_replies() {
        assert!(parse_reply(&[0_u8; 20]).is_none(), "short reply");
        let mut kiss_of_death = reply_for(1_609_459_200);
        kiss_of_death[1] = 0; // stratum 0
        assert!(parse_reply(&kiss_of_death).is_none(), "kiss of death");
        let mut mirrored = reply_for(1_609_459_200);
        mirrored[0] = 0x1b; // our own request echoed back
        assert!(parse_reply(&mirrored).is_none(), "client mode reply");
        assert!(parse_reply(&[0_u8; 48]).is_none(), "no timestamp");
    }

    #[test]
    fn reads_the_default_gateway() {
        let routes = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                      wlan0\t00000000\t0102A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
        assert_eq!(gateway_from(routes).as_deref(), Some("192.168.2.1"));
        assert_eq!(gateway_from("Iface\tDestination\tGateway\n"), None);
    }
}
