//! Minimal ONVIF SOAP adapter on the existing authenticated HTTPS listener.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use roxmltree::{Document, Node};
use sha1::{Digest, Sha1};
use std::collections::VecDeque;
use std::time::Instant;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zeroize::Zeroizing;

const SOAP: &str = "http://www.w3.org/2003/05/soap-envelope";
const DEVICE: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
const PTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
const SCHEMA: &str = "http://www.onvif.org/ver10/schema";
const WSSE: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd";
const WSU: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd";
const PASSWORD_DIGEST: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest";
const VELOCITY_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace";
/// One logical camera/profile, configuration and physical PTZ node.
const TOKEN: &str = "c220";
/// Match the existing MQTT lost-release safety policy; no move outlives ten seconds.
const MOVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum bounded movement request matches the driver's 100 ms position poll.
const MIN_MOVE_TIMEOUT: Duration = Duration::from_millis(100);
/// Map normalized ONVIF velocities onto the existing public 0..10 PTZ scale.
const SPEED_SCALE: f32 = 10.0;
/// RFC3339 date/time including fractional seconds and an explicit UTC offset.
const MAX_CREATED_LEN: usize = 40;
/// WS-Security permits five minutes of clock skew; replay entries live twice
/// that window to cover a Created timestamp initially ahead of our clock.
const CLOCK_SKEW_SECONDS: i64 = 300;
const REPLAY_LIFETIME: Duration = Duration::from_secs(600);
/// Fail closed when the bounded authentication replay table fills.
const REPLAY_CAPACITY: usize = 4096;
/// Bound parsed XML nodes within the existing 64 KiB HTTP body limit.
const XML_NODE_LIMIT: u32 = 2048;
/// WSSE nonces are normally sixteen bytes; reject large retained/auth inputs.
const NONCE_BYTES: usize = 64;

pub struct Service {
    username: String,
    // WSSE PasswordDigest needs the original secret, not HTTP Digest's HA1.
    password: Zeroizing<String>,
    rtsp_port: u16,
    replay: Mutex<VecDeque<([u8; 20], Instant)>>,
}

impl Service {
    pub fn new(username: String, password: Zeroizing<String>, rtsp_port: u16) -> Self {
        Self {
            username,
            password,
            rtsp_port,
            replay: Mutex::new(VecDeque::new()),
        }
    }

    fn authenticate(&self, document: &Document<'_>) -> bool {
        let Some(header) = document
            .root_element()
            .children()
            .find(|n| n.has_tag_name((SOAP, "Header")))
        else {
            return false;
        };
        let tokens: Vec<_> = header
            .descendants()
            .filter(|n| n.has_tag_name((WSSE, "UsernameToken")))
            .collect();
        let [token] = tokens.as_slice() else {
            return false;
        };
        let username = child(*token, WSSE, "Username").and_then(|n| n.text());
        let password = child(*token, WSSE, "Password");
        let nonce = child(*token, WSSE, "Nonce").and_then(|n| n.text());
        let created = child(*token, WSU, "Created").and_then(|n| n.text());
        let (Some(username), Some(password), Some(nonce), Some(created)) =
            (username, password, nonce, created)
        else {
            return false;
        };
        if username != self.username
            || password.attribute("Type") != Some(PASSWORD_DIGEST)
            || created.len() > MAX_CREATED_LEN
            || nonce.len() > 4 * NONCE_BYTES
        {
            return false;
        }
        let Ok(stamp) = OffsetDateTime::parse(created, &Rfc3339) else {
            return false;
        };
        if (OffsetDateTime::now_utc().unix_timestamp() - stamp.unix_timestamp()).abs()
            > CLOCK_SKEW_SECONDS
        {
            return false;
        }
        let Ok(nonce) = STANDARD.decode(nonce) else {
            return false;
        };
        if nonce.is_empty() || nonce.len() > NONCE_BYTES {
            return false;
        }
        let mut hash = Sha1::new();
        hash.update(&nonce);
        hash.update(created.as_bytes());
        let replay_key: [u8; 20] = hash.clone().finalize().into();
        hash.update(self.password.trim().as_bytes());
        let expected = STANDARD.encode(hash.finalize());
        if !crate::digest::constant_time_eq(password.text().unwrap_or(""), &expected) {
            return false;
        }
        let Ok(mut replay) = self.replay.lock() else {
            return false;
        };
        let now = Instant::now();
        replay.retain(|(_, seen)| now.duration_since(*seen) < REPLAY_LIFETIME);
        if replay.len() == REPLAY_CAPACITY || replay.iter().any(|(key, _)| *key == replay_key) {
            return false;
        }
        replay.push_back((replay_key, now));
        true
    }

    pub(super) async fn dispatch(
        &self,
        inner: &Arc<ControlInner>,
        body: &[u8],
        peer: SocketAddr,
        local: SocketAddr,
    ) -> Vec<u8> {
        let Ok(xml) = std::str::from_utf8(body) else {
            return fault("InvalidArgVal", "Invalid XML");
        };
        let options = roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: XML_NODE_LIMIT,
            ..Default::default()
        };
        let Ok(document) = Document::parse_with_options(xml, options) else {
            return fault("InvalidArgVal", "Invalid XML");
        };
        let envelope = document.root_element();
        if !envelope.has_tag_name((SOAP, "Envelope")) {
            return fault("InvalidArgVal", "SOAP 1.2 required");
        }
        let bodies: Vec<_> = envelope
            .children()
            .filter(|n| n.has_tag_name((SOAP, "Body")))
            .collect();
        let [soap_body] = bodies.as_slice() else {
            return fault("InvalidArgVal", "One SOAP body required");
        };
        let actions: Vec<_> = soap_body.children().filter(Node::is_element).collect();
        let [action] = actions.as_slice() else {
            return fault("InvalidArgVal", "One SOAP operation required");
        };
        let action = *action;
        let name = action.tag_name().name();
        let namespace = action.tag_name().namespace().unwrap_or("");
        // ONVIF permits unauthenticated clock queries so clients can build WSSE tokens.
        if !(namespace == DEVICE && name == "GetSystemDateAndTime") {
            if inner.lockout.is_locked(peer.ip()).await {
                return fault("NotAuthorized", "Authentication temporarily locked");
            }
            if !self.authenticate(&document) {
                if document
                    .descendants()
                    .any(|n| n.has_tag_name((WSSE, "UsernameToken")))
                {
                    inner.lockout.record_failure(peer.ip()).await;
                }
                return fault("NotAuthorized", "Authentication required");
            }
            inner.lockout.record_success(peer.ip()).await;
        }
        if inner.shutdown.is_stopped() {
            return fault("ActionNotSupported", "Service stopped");
        }
        let endpoint = format!("https://{local}");
        let stream_uri = format!(
            "rtsps://{}/live",
            SocketAddr::new(local.ip(), self.rtsp_port)
        );
        // C220's validated native capture profile is fixed at 2304×1296/15 fps.
        // Report its real HEVC encoding, never invent an H.264 stream for ONVIF.
        let result = match (namespace, name) {
            (DEVICE, "GetSystemDateAndTime") => {
                let now = OffsetDateTime::now_utc();
                format!("<tds:SystemDateAndTime><tt:DateTimeType>Manual</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings><tt:UTCDateTime><tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time><tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime>", now.hour(), now.minute(), now.second(), now.year(), u8::from(now.month()), now.day())
            }
            (DEVICE, "GetDeviceInformation") => "<tds:Manufacturer>Standalone</tds:Manufacturer><tds:Model>C220</tds:Model><tds:FirmwareVersion>0.1.0</tds:FirmwareVersion><tds:SerialNumber>c220</tds:SerialNumber><tds:HardwareId>C220</tds:HardwareId>".into(),
            (DEVICE, "GetCapabilities") => format!("<tds:Capabilities><tt:Device><tt:XAddr>{}/onvif/device_service</tt:XAddr><tt:System><tt:SupportedVersions><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tt:SupportedVersions></tt:System></tt:Device><tt:Media><tt:XAddr>{}/onvif/media_service</tt:XAddr><tt:StreamingCapabilities><tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP><tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media><tt:PTZ><tt:XAddr>{}/onvif/ptz_service</tt:XAddr></tt:PTZ></tds:Capabilities>", endpoint, endpoint, endpoint),
            (DEVICE, "GetServices") => [ (DEVICE,"device_service"), (MEDIA,"media_service"), (PTZ,"ptz_service") ].iter().map(|(ns,path)| format!("<tds:Service><tds:Namespace>{ns}</tds:Namespace><tds:XAddr>{}/onvif/{path}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version></tds:Service>",endpoint)).collect(),
            (MEDIA, "GetProfiles") => format!("<trt:Profiles token=\"{TOKEN}\" fixed=\"true\"><tt:Name>C220</tt:Name><tt:VideoSourceConfiguration token=\"{TOKEN}\"><tt:Name>C220</tt:Name><tt:UseCount>1</tt:UseCount><tt:SourceToken>{TOKEN}</tt:SourceToken><tt:Bounds x=\"0\" y=\"0\" width=\"2304\" height=\"1296\"/></tt:VideoSourceConfiguration><tt:VideoEncoderConfiguration token=\"{TOKEN}\"><tt:Name>HEVC</tt:Name><tt:UseCount>1</tt:UseCount><tt:Encoding>H265</tt:Encoding><tt:Resolution><tt:Width>2304</tt:Width><tt:Height>1296</tt:Height></tt:Resolution><tt:Quality>1</tt:Quality><tt:SessionTimeout>PT60S</tt:SessionTimeout></tt:VideoEncoderConfiguration>{}</trt:Profiles>", configuration("tt:PTZConfiguration")),
            (MEDIA, "GetVideoSources") => format!("<trt:VideoSources token=\"{TOKEN}\"><tt:Framerate>15</tt:Framerate><tt:Resolution><tt:Width>2304</tt:Width><tt:Height>1296</tt:Height></tt:Resolution></trt:VideoSources>"),
            (MEDIA, "GetStreamUri") => {
                if !valid_token(action, MEDIA, "ProfileToken") { return fault("InvalidArgVal", "Unknown profile"); }
                format!("<trt:MediaUri><tt:Uri>{}</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT60S</tt:Timeout></trt:MediaUri>",stream_uri)
            }
            (PTZ, "GetConfigurations") => configuration("tptz:PTZConfiguration"),
            (PTZ, "GetConfiguration") => {
                if !valid_token(action, PTZ, "PTZConfigurationToken") { return fault("InvalidArgVal", "Unknown configuration"); }
                configuration("tptz:PTZConfiguration")
            }
            (PTZ, "GetConfigurationOptions") => {
                if !valid_token(action, PTZ, "ConfigurationToken") { return fault("InvalidArgVal", "Unknown configuration"); }
                format!("<tptz:PTZConfigurationOptions><tt:Spaces>{}</tt:Spaces><tt:PTZTimeout><tt:Min>PT0.1S</tt:Min><tt:Max>PT10S</tt:Max></tt:PTZTimeout></tptz:PTZConfigurationOptions>", velocity_space())
            }
            (PTZ, "GetPresets") => {
                if !valid_token(action, PTZ, "ProfileToken") { return fault("InvalidArgVal", "Unknown profile"); }
                String::new()
            }
            (PTZ, "ContinuousMove") => {
                if !valid_token(action, PTZ, "ProfileToken") { return fault("InvalidArgVal", "Unknown profile"); }
                let Ok((direction,speed,timeout)) = movement(action) else { return fault("InvalidArgVal", "Use one finite pan/tilt axis in [-1,1], no zoom, timeout 0.1-10 seconds"); };
                if direction == PtzDirection::Stop {
                    if send_ptz(inner, |ptz| ptz.stop()).await.is_err() { return fault("ActionNotSupported", "PTZ unavailable"); }
                } else {
                    let Ok(motion) = send_ptz(inner, move |ptz| ptz.start_owned(direction,speed)).await else { return fault("ActionNotSupported", "PTZ unavailable"); };
                    let mut shutdown = inner.shutdown();
                    tokio::spawn(async move {
                        tokio::select! { biased; _ = shutdown.wait() => {}, _ = tokio::time::sleep(timeout) => {} }
                        let _ = tokio::task::spawn_blocking(move || motion.stop()).await;
                    });
                }
                String::new()
            }
            (PTZ, "Stop") => {
                if !valid_token(action, PTZ, "ProfileToken") { return fault("InvalidArgVal", "Unknown profile"); }
                if send_ptz(inner, |ptz| ptz.stop()).await.is_err() { return fault("ActionNotSupported", "PTZ unavailable"); }
                String::new()
            }
            _ => return fault("ActionNotSupported", "Operation not supported"),
        };
        let prefix = match namespace {
            DEVICE => "tds",
            MEDIA => "trt",
            _ => "tptz",
        };
        response(
            200,
            &format!("<{prefix}:{name}Response>{result}</{prefix}:{name}Response>"),
        )
    }
}

fn child<'a, 'input>(
    node: Node<'a, 'input>,
    namespace: &str,
    name: &str,
) -> Option<Node<'a, 'input>> {
    node.children().find(|n| n.has_tag_name((namespace, name)))
}
fn valid_token(node: Node<'_, '_>, ns: &str, name: &str) -> bool {
    child(node, ns, name).and_then(|n| n.text()) == Some(TOKEN)
}
fn velocity_space() -> String {
    format!(
        "<tt:ContinuousPanTiltVelocitySpace><tt:URI>{VELOCITY_SPACE}</tt:URI><tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange><tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange></tt:ContinuousPanTiltVelocitySpace>"
    )
}
fn configuration(tag: &str) -> String {
    format!(
        "<{tag} token=\"{TOKEN}\"><tt:Name>C220</tt:Name><tt:UseCount>1</tt:UseCount><tt:NodeToken>{TOKEN}</tt:NodeToken><tt:DefaultContinuousPanTiltVelocitySpace>{VELOCITY_SPACE}</tt:DefaultContinuousPanTiltVelocitySpace><tt:DefaultPTZTimeout>PT10S</tt:DefaultPTZTimeout></{tag}>"
    )
}
fn movement(action: Node<'_, '_>) -> Result<(PtzDirection, u8, Duration), ()> {
    let velocity = child(action, PTZ, "Velocity").ok_or(())?;
    if child(velocity, SCHEMA, "Zoom").is_some() {
        return Err(());
    }
    let pan_tilt = child(velocity, SCHEMA, "PanTilt").ok_or(())?;
    if pan_tilt
        .attribute("space")
        .is_some_and(|s| s != VELOCITY_SPACE)
    {
        return Err(());
    }
    let x: f32 = pan_tilt.attribute("x").ok_or(())?.parse().map_err(|_| ())?;
    let y: f32 = pan_tilt.attribute("y").ok_or(())?.parse().map_err(|_| ())?;
    if !x.is_finite() || !y.is_finite() || x.abs() > 1.0 || y.abs() > 1.0 || (x != 0.0 && y != 0.0)
    {
        return Err(());
    }
    let direction = if x < 0.0 {
        PtzDirection::Left
    } else if x > 0.0 {
        PtzDirection::Right
    } else if y > 0.0 {
        PtzDirection::Up
    } else if y < 0.0 {
        PtzDirection::Down
    } else {
        PtzDirection::Stop
    };
    let timeout = match child(action, PTZ, "Timeout").and_then(|n| n.text()) {
        None => MOVE_TIMEOUT,
        Some(text) => {
            let seconds: f64 = text
                .strip_prefix("PT")
                .and_then(|s| s.strip_suffix('S'))
                .ok_or(())?
                .parse()
                .map_err(|_| ())?;
            if !seconds.is_finite()
                || !(MIN_MOVE_TIMEOUT.as_secs_f64()..=MOVE_TIMEOUT.as_secs_f64()).contains(&seconds)
            {
                return Err(());
            }
            Duration::from_secs_f64(seconds)
        }
    };
    Ok((
        direction,
        (x.abs().max(y.abs()) * SPEED_SCALE).round() as u8,
        timeout,
    ))
}
fn response(status: u16, body: &str) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"{SOAP}\" xmlns:tds=\"{DEVICE}\" xmlns:trt=\"{MEDIA}\" xmlns:tptz=\"{PTZ}\" xmlns:tt=\"{SCHEMA}\" xmlns:ter=\"http://www.onvif.org/ver10/error\"><s:Body>{body}</s:Body></s:Envelope>"
    );
    let reason = if status == 200 {
        "OK"
    } else {
        "Internal Server Error"
    };
    let mut data = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/soap+xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",xml.len()).into_bytes();
    data.extend_from_slice(xml.as_bytes());
    data
}
fn fault(code: &str, reason: &str) -> Vec<u8> {
    response(
        500,
        &format!(
            "<s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:{code}</s:Value></s:Subcode></s:Code><s:Reason><s:Text xml:lang=\"en\">{reason}</s:Text></s:Reason></s:Fault>"
        ),
    )
}

#[cfg(test)]
mod tests;
