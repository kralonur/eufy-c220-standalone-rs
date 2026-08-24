//! HTTP and RTSP Digest authentication for the camera-local services.
//!
//! Passwords are read from writable NVRAM configuration and are never emitted
//! in challenges or logs. The nonce is authenticated with a process-local key
//! and expires, so a captured digest response cannot be replayed indefinitely.

use hmac::{Hmac, Mac};
use md5::{Digest as _, Md5};
use sha2::Sha256;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

/// Digest nonce lifetime. It is long enough for a client handshake but short
/// enough to limit replay of a captured response.
const NONCE_MAX_AGE: Duration = Duration::from_secs(300);

/// Shared authentication realm used by the HTTP and RTSP services.
pub const REALM: &str = "eufy-c220";

/// Digest algorithm offered by a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Sha256,
    Md5,
}

impl Algorithm {
    pub const fn token(self) -> &'static str {
        match self {
            Self::Sha256 => "SHA-256",
            Self::Md5 => "MD5",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token.trim().trim_matches('"').to_ascii_uppercase().as_str() {
            "SHA-256" | "SHA256" => Some(Self::Sha256),
            "MD5" => Some(Self::Md5),
            _ => None,
        }
    }

    fn hash(self, data: &[u8]) -> String {
        match self {
            Self::Sha256 => hex(&Sha256::digest(data)),
            Self::Md5 => hex(&Md5::digest(data)),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

/// Result of checking an Authorization header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Authorized,
    Stale,
    Denied,
}

/// Credentials and nonce state for one protected service.
pub struct DigestAuth {
    username: String,
    password: String,
    nonce_secret: [u8; 32],
    max_nonce_age: Duration,
}

impl DigestAuth {
    /// Construct an authenticator using the camera's CSPRNG.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> io::Result<Self> {
        let username = username.into();
        let password = password.into();
        if username.trim().is_empty() || password.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Digest username and password must not be empty",
            ));
        }
        let mut nonce_secret = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut nonce_secret)?;
        Ok(Self {
            username,
            password,
            nonce_secret,
            max_nonce_age: NONCE_MAX_AGE,
        })
    }

    #[cfg(test)]
    fn with_nonce_max_age(mut self, max_age: Duration) -> Self {
        self.max_nonce_age = max_age;
        self
    }

    /// Build a single `WWW-Authenticate` challenge.
    pub fn challenge(&self, now: SystemTime, algorithm: Algorithm) -> String {
        let nonce = self.nonce(now);
        format!(
            "Digest realm=\"{REALM}\", qop=\"auth\", algorithm={}, nonce=\"{nonce}\", opaque=\"{}\"",
            algorithm.token(),
            hex(&self.nonce_secret[..8])
        )
    }

    /// Build both HTTP challenges. RTSP uses only MD5 because several common
    /// clients reject a response containing multiple algorithm choices.
    pub fn challenges(&self, now: SystemTime) -> [String; 2] {
        [
            self.challenge(now, Algorithm::Sha256),
            self.challenge(now, Algorithm::Md5),
        ]
    }

    fn nonce(&self, now: SystemTime) -> String {
        let stamp = unix_seconds(now);
        format!("{stamp}:{}", self.nonce_mac(stamp))
    }

    fn nonce_mac(&self, stamp: u64) -> String {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.nonce_secret)
            .expect("HMAC accepts every key length");
        mac.update(stamp.to_string().as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    /// Verify one HTTP or RTSP Digest Authorization value.
    pub fn authorize(
        &self,
        method: &str,
        uri: &str,
        authorization: &str,
        now: SystemTime,
    ) -> Outcome {
        let Some(fields) = parse_digest_header(authorization) else {
            return Outcome::Denied;
        };
        if fields
            .get("username")
            .is_none_or(|value| !constant_time_eq(value, &self.username))
        {
            return Outcome::Denied;
        }
        if fields
            .get("realm")
            .is_none_or(|value| !constant_time_eq(value, REALM))
        {
            return Outcome::Denied;
        }
        if fields
            .get("uri")
            .is_none_or(|value| !constant_time_eq(value, uri))
        {
            return Outcome::Denied;
        }
        let Some(nonce) = fields.get("nonce") else {
            return Outcome::Denied;
        };
        match self.check_nonce(nonce, now) {
            NonceCheck::Valid => {}
            NonceCheck::Stale => return Outcome::Stale,
            NonceCheck::Invalid => return Outcome::Denied,
        }
        let Some(response) = fields.get("response") else {
            return Outcome::Denied;
        };
        let algorithm = match fields.get("algorithm") {
            Some(value) => match Algorithm::from_token(value) {
                Some(algorithm) => algorithm,
                None => return Outcome::Stale,
            },
            None => Algorithm::Md5,
        };
        let ha1 = algorithm.hash(format!("{}:{REALM}:{}", self.username, self.password).as_bytes());
        let ha2 = algorithm.hash(format!("{method}:{uri}").as_bytes());
        let expected = match fields.get("qop") {
            Some(qop) if qop.eq_ignore_ascii_case("auth") => {
                let (Some(nc), Some(cnonce)) = (fields.get("nc"), fields.get("cnonce")) else {
                    return Outcome::Denied;
                };
                algorithm.hash(format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}").as_bytes())
            }
            None => algorithm.hash(format!("{ha1}:{nonce}:{ha2}").as_bytes()),
            Some(_) => return Outcome::Denied,
        };
        if constant_time_eq(response, &expected) {
            Outcome::Authorized
        } else {
            Outcome::Denied
        }
    }

    fn check_nonce(&self, nonce: &str, now: SystemTime) -> NonceCheck {
        let Some((stamp, mac)) = nonce.split_once(':') else {
            return NonceCheck::Invalid;
        };
        let Ok(stamp) = stamp.parse::<u64>() else {
            return NonceCheck::Invalid;
        };
        if !constant_time_eq(mac, &self.nonce_mac(stamp)) {
            return NonceCheck::Invalid;
        }
        let now = unix_seconds(now);
        if now < stamp || now.saturating_sub(stamp) > self.max_nonce_age.as_secs() {
            return NonceCheck::Stale;
        }
        NonceCheck::Valid
    }
}

enum NonceCheck {
    Valid,
    Stale,
    Invalid,
}

fn unix_seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Parse `Digest key=value, key="value"` without splitting commas inside quotes.
fn parse_digest_header(header: &str) -> Option<HashMap<String, String>> {
    let mut parts = header.splitn(2, |character: char| character.is_ascii_whitespace());
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("Digest") {
        return None;
    }
    let rest = parts.next()?.trim_start();
    let mut fields = HashMap::new();
    let bytes = rest.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && matches!(bytes[index], b' ' | b',') {
            index += 1;
        }
        let key_start = index;
        while index < bytes.len() && bytes[index] != b'=' && bytes[index] != b',' {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            return None;
        }
        let key = rest[key_start..index].trim().to_ascii_lowercase();
        if key.is_empty() {
            return None;
        }
        index += 1;
        let value = if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            let value_start = index;
            while index < bytes.len() && bytes[index] != b'"' {
                if bytes[index] == b'\\' {
                    // Digest values used here are ASCII tokens/URIs; reject
                    // escaped syntax rather than accepting an ambiguous header.
                    return None;
                }
                index += 1;
            }
            if index >= bytes.len() {
                return None;
            }
            let value = rest[value_start..index].to_string();
            index += 1;
            value
        } else {
            let value_start = index;
            while index < bytes.len() && bytes[index] != b',' {
                index += 1;
            }
            let value = rest[value_start..index].trim();
            if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
                return None;
            }
            value.to_string()
        };
        if fields.insert(key, value).is_some() {
            return None;
        }
    }
    (!fields.is_empty()).then_some(fields)
}

const MAX_LOCKOUT_ENTRIES: usize = 64;

/// Per-client brute-force protection shared by HTTP and RTSP.
pub struct Lockout {
    state: Mutex<HashMap<IpAddr, Attempts>>,
    max_failures: u32,
    lockout: Duration,
}

struct Attempts {
    failures: u32,
    locked_until: Option<std::time::Instant>,
    last_seen: std::time::Instant,
}

impl Default for Lockout {
    fn default() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            max_failures: 5,
            lockout: Duration::from_secs(30 * 60),
        }
    }
}

impl Lockout {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_policy(max_failures: u32, lockout: Duration) -> Self {
        Self {
            max_failures,
            lockout,
            ..Self::default()
        }
    }

    /// Return true while the address is locked out.
    pub async fn is_locked(&self, ip: IpAddr) -> bool {
        let mut state = self.state.lock().await;
        let Some(attempts) = state.get_mut(&ip) else {
            return false;
        };
        match attempts.locked_until {
            Some(until) if until > std::time::Instant::now() => true,
            Some(_) => {
                state.remove(&ip);
                false
            }
            None => false,
        }
    }

    /// Count one failed credential attempt.
    pub async fn record_failure(&self, ip: IpAddr) {
        let mut state = self.state.lock().await;
        let now = std::time::Instant::now();
        if state.len() >= MAX_LOCKOUT_ENTRIES && !state.contains_key(&ip) {
            state.retain(|_, attempts| {
                if let Some(until) = attempts.locked_until {
                    until > now
                } else {
                    now.duration_since(attempts.last_seen) < self.lockout
                }
            });
            if state.len() >= MAX_LOCKOUT_ENTRIES
                && let Some((&oldest, _)) =
                    state.iter().min_by_key(|(_, attempts)| attempts.last_seen)
            {
                state.remove(&oldest);
            }
        }
        let attempts = state.entry(ip).or_insert(Attempts {
            failures: 0,
            locked_until: None,
            last_seen: now,
        });
        if let Some(until) = attempts.locked_until {
            if until > now {
                // Requests that raced with the lock check must not prolong it.
                return;
            }
            attempts.locked_until = None;
            attempts.failures = 0;
        }
        attempts.last_seen = now;
        attempts.failures += 1;
        if attempts.failures >= self.max_failures {
            attempts.locked_until = Some(now + self.lockout);
            attempts.failures = 0;
        }
    }

    /// Clear the failure budget after a valid request.
    pub async fn record_success(&self, ip: IpAddr) {
        self.state.lock().await.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "camera";
    const PASSWORD: &str = "s3cret-passphrase";
    const URI: &str = "/live";

    fn client_header(
        auth: &DigestAuth,
        method: &str,
        uri: &str,
        password: &str,
        algorithm: Algorithm,
        qop: bool,
        now: SystemTime,
    ) -> String {
        let nonce = auth.nonce(now);
        let ha1 = algorithm.hash(format!("{USER}:{REALM}:{password}").as_bytes());
        let ha2 = algorithm.hash(format!("{method}:{uri}").as_bytes());
        let cnonce = "0a4f113b";
        let nc = "00000001";
        let response = if qop {
            algorithm.hash(format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}").as_bytes())
        } else {
            algorithm.hash(format!("{ha1}:{nonce}:{ha2}").as_bytes())
        };
        let mut header = format!(
            "Digest username=\"{USER}\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm={}",
            algorithm.token()
        );
        if qop {
            header.push_str(&format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\""));
        }
        header
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    #[test]
    fn accepts_supported_algorithms_and_qop_forms() {
        let auth = DigestAuth::new(USER, PASSWORD).unwrap();
        for algorithm in [Algorithm::Sha256, Algorithm::Md5] {
            for qop in [true, false] {
                let header = client_header(&auth, "DESCRIBE", URI, PASSWORD, algorithm, qop, now());
                assert_eq!(
                    auth.authorize("DESCRIBE", URI, &header, now()),
                    Outcome::Authorized
                );
            }
        }
    }

    #[test]
    fn rejects_wrong_password_and_request_binding() {
        let auth = DigestAuth::new(USER, PASSWORD).unwrap();
        let wrong = client_header(
            &auth,
            "DESCRIBE",
            URI,
            "guess",
            Algorithm::Sha256,
            true,
            now(),
        );
        assert_eq!(
            auth.authorize("DESCRIBE", URI, &wrong, now()),
            Outcome::Denied
        );
        let right = client_header(
            &auth,
            "DESCRIBE",
            URI,
            PASSWORD,
            Algorithm::Sha256,
            true,
            now(),
        );
        assert_eq!(
            auth.authorize("DESCRIBE", "/other", &right, now()),
            Outcome::Denied
        );
        let mismatched_uri = right.replace("uri=\"/live\"", "uri=\"/other\"");
        assert_eq!(
            auth.authorize("DESCRIBE", URI, &mismatched_uri, now()),
            Outcome::Denied
        );
        assert_eq!(auth.authorize("SETUP", URI, &right, now()), Outcome::Denied);
    }

    #[test]
    fn malformed_or_ambiguous_headers_are_rejected() {
        assert!(parse_digest_header("Digest username=one, username=two").is_none());
        assert!(parse_digest_header("Digest username=\"unterminated").is_none());
        assert!(parse_digest_header("Digest username=one trailing").is_none());
    }

    #[test]
    fn expired_nonce_is_stale() {
        let auth = DigestAuth::new(USER, PASSWORD)
            .unwrap()
            .with_nonce_max_age(Duration::from_secs(30));
        let header = client_header(
            &auth,
            "DESCRIBE",
            URI,
            PASSWORD,
            Algorithm::Sha256,
            true,
            now(),
        );
        assert_eq!(
            auth.authorize("DESCRIBE", URI, &header, now() + Duration::from_secs(31)),
            Outcome::Stale
        );
    }

    #[tokio::test]
    async fn lockout_starts_at_the_configured_failure_count() {
        let lockout = Lockout::with_policy(2, Duration::from_secs(60));
        let ip = "192.0.2.10".parse().unwrap();
        lockout.record_failure(ip).await;
        assert!(!lockout.is_locked(ip).await);
        lockout.record_failure(ip).await;
        assert!(lockout.is_locked(ip).await);
        lockout.record_success(ip).await;
        assert!(!lockout.is_locked(ip).await);
    }
}
