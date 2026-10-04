//! MQTT control, telemetry and optional Home Assistant discovery.
//!
//! The broker is contacted only when a host is configured, so a camera that has
//! never been provisioned makes no outbound connection at all. Credentials and the
//! broker's CA live in NVRAM; the password file is checked the same way the RTSP
//! password is, because a world-readable secret is not a secret.
//!
//! Commands take the same path as the HTTP control API: this module calls the same
//! handlers rather than reimplementing them, so a PTZ command over MQTT cannot
//! behave differently from the same command over HTTPS.

#[cfg(test)]
mod tests;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::time::{interval, sleep};
use tracing::{info, warn};

use crate::control::{
    ControlInner, night_mode, ptz_center, ptz_move, ptz_rotate_owned, ptz_stop, status_response,
    talkback_start, talkback_status_response, talkback_stop,
};
use crate::security;

/// Broker host, mapped from the boot script. Without it there is no MQTT at all.
const HOST_ENV: &str = "MQTT_HOST";
const PORT_ENV: &str = "MQTT_PORT";
const USERNAME_ENV: &str = "MQTT_USERNAME";
const PASSWORD_FILE_ENV: &str = "MQTT_PASSWORD_FILE";
const CA_FILE_ENV: &str = "MQTT_CA_FILE";
const TOPIC_PREFIX_ENV: &str = "MQTT_TOPIC_PREFIX";
const DISCOVERY_PREFIX_ENV: &str = "MQTT_DISCOVERY_PREFIX";
const DISCOVERY_ENV: &str = "MQTT_DISCOVERY";

/// IANA MQTT-over-TLS port, used when the configuration does not name one.
const DEFAULT_TLS_PORT: u16 = 8883;

/// Prefix used when the configuration does not name one.
const DEFAULT_TOPIC_PREFIX: &str = "eufy_c220";
const DEFAULT_DISCOVERY_PREFIX: &str = "homeassistant";

/// How long a `ptz/hold` may drive toward a calibrated limit without a release
/// message. Its generation-owned stop cannot interrupt a newer command.
const HOLD_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval for superseded holds; service closure wakes them immediately.
const HOLD_STEP_INTERVAL: Duration = Duration::from_millis(200);
/// PTZ speed used for every MQTT command, on the 0-10 scale.
const COMMAND_SPEED: u8 = 5;

/// How often telemetry is published.
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(10);
/// Delay before reconnecting after a broker or connection failure.
const RECONNECT_DELAY: Duration = Duration::from_secs(10);

/// Broker settings read from the environment.
pub struct MqttConfig {
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<Zeroizing<String>>,
    ca_file: PathBuf,
    topic_prefix: String,
    discovery_prefix: String,
    discovery_enabled: bool,
    client_id: String,
}

impl MqttConfig {
    /// Read the broker settings, or `None` when no host is configured.
    pub fn from_env() -> io::Result<Option<Self>> {
        let Some(host) = environment(HOST_ENV)? else {
            return Ok(None);
        };
        let ca_file = require_ca_file(environment(CA_FILE_ENV)?.map(PathBuf::from))?;
        let port = match environment(PORT_ENV)? {
            Some(value) => value.trim().parse::<u16>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{PORT_ENV} must be a port number: {error}"),
                )
            })?,
            None => DEFAULT_TLS_PORT,
        };
        let username = environment(USERNAME_ENV)?;
        let password = security::secret_file(PASSWORD_FILE_ENV)?
            .map(|password| Zeroizing::new(password.trim().to_string()));
        if username.is_some() != password.is_some()
            || password
                .as_deref()
                .is_some_and(|password| password.is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MQTT username and nonempty password file must be configured together",
            ));
        }
        // The camera's own derived name makes the client id unique per camera, which
        // is what keeps two cameras from displacing each other on the broker.
        let client_id = format!("eufy-c220-{}", security::host_label("HOST_NAME", "c220")?);
        Ok(Some(Self {
            host,
            port,
            username,
            password,
            ca_file,
            topic_prefix: environment(TOPIC_PREFIX_ENV)?
                .unwrap_or_else(|| DEFAULT_TOPIC_PREFIX.to_string()),
            discovery_prefix: environment(DISCOVERY_PREFIX_ENV)?
                .unwrap_or_else(|| DEFAULT_DISCOVERY_PREFIX.to_string()),
            discovery_enabled: security::flag(DISCOVERY_ENV, false)?,
            client_id,
        }))
    }
}

fn require_ca_file(ca_file: Option<PathBuf>) -> io::Result<PathBuf> {
    let path = ca_file.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{CA_FILE_ENV} is required when {HOST_ENV} is configured"),
        )
    })?;
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MQTT CA path must be absolute",
        ));
    }
    Ok(path)
}

fn environment(variable: &str) -> io::Result<Option<String>> {
    match std::env::var(variable) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{variable} is not valid UTF-8"),
        )),
    }
}

/// The body of an HTTP-style response produced by the shared control handlers.
fn response_body(response: &[u8]) -> &str {
    let text = std::str::from_utf8(response).unwrap_or_default();
    match text.find("\r\n\r\n") {
        Some(index) => text[index + 4..].trim(),
        None => text.trim(),
    }
}

/// The MQTT session, sharing the control plane the HTTP API uses.
pub(crate) struct MqttTask {
    config: MqttConfig,
    control: Arc<ControlInner>,
    /// Set while a `ptz/hold` stepping loop is running, so a second hold replaces it
    /// rather than racing it.
    holding: Arc<AtomicU64>,
}

impl MqttTask {
    pub(crate) fn new(config: MqttConfig, control: Arc<ControlInner>) -> Self {
        Self {
            config,
            control,
            holding: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Run until the process ends, reconnecting after failures.
    pub(crate) async fn run(self) {
        let mut shutdown = self.control.shutdown();
        loop {
            let result = tokio::select! {
                biased;
                _ = shutdown.wait() => return,
                result = self.session() => result,
            };
            if let Err(error) = result {
                warn!(%error, "MQTT session ended; reconnecting");
            }
            tokio::select! {
                biased;
                _ = shutdown.wait() => return,
                _ = sleep(RECONNECT_DELAY) => {},
            }
        }
    }

    async fn session(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let prefix = &self.config.topic_prefix;
        let mut options =
            MqttOptions::new(&self.config.client_id, &self.config.host, self.config.port);
        options.set_keep_alive(Duration::from_secs(30));

        let ca_file = &self.config.ca_file;
        let ca = std::fs::read(ca_file).map_err(|error| {
            io::Error::other(format!(
                "MQTT CA file {} is unreadable: {error}",
                ca_file.display()
            ))
        })?;
        options.set_transport(rumqttc::Transport::tls_with_config(
            rumqttc::TlsConfiguration::Simple {
                ca,
                alpn: None,
                client_auth: None,
            },
        ));
        info!(ca = %ca_file.display(), port = self.config.port, "MQTT over TLS");

        // Anything watching the broker learns the camera is gone even if it loses
        // power, because the broker publishes this on the camera's behalf.
        options.set_last_will(rumqttc::LastWill::new(
            format!("{prefix}/availability"),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));
        if let Some(username) = &self.config.username {
            options.set_credentials(
                username,
                self.config
                    .password
                    .as_ref()
                    .map_or("", |password| password.as_str()),
            );
        }

        let (client, mut eventloop) = AsyncClient::new(options, 64);
        for topic in [
            format!("{prefix}/ptz/set"),
            format!("{prefix}/ptz/hold"),
            format!("{prefix}/ptz/center"),
            // The dashboard's PTZ card drives these two; without them a held button
            // and a recentre command go nowhere at all.
            format!("{prefix}/ptz/move"),
            format!("{prefix}/ptz/calibrate"),
            format!("{prefix}/night_mode/set"),
            format!("{prefix}/talkback/set"),
        ] {
            client.subscribe(topic, QoS::AtLeastOnce).await?;
        }

        let mut ticker = interval(TELEMETRY_INTERVAL);
        loop {
            tokio::select! {
                event = eventloop.poll() => match event? {
                    Event::Incoming(Incoming::ConnAck(_)) => {
                        info!(host = %self.config.host, port = self.config.port, "MQTT connected");
                        client
                            .publish(format!("{prefix}/availability"), QoS::AtLeastOnce, true, "online")
                            .await?;
                        // Every connection, not just the first: the messages are retained
                        // and idempotent, so a broker that lost its retained store - or that
                        // was only just granted write access to the discovery prefix - repairs
                        // itself on the camera's next reconnect instead of waiting for a reboot.
                        if self.config.discovery_enabled {
                            self.publish_discovery(&client).await?;
                        }
                        self.publish_state(&client).await?;
                    }
                    Event::Incoming(Incoming::Publish(publish)) => {
                        self.handle_command(&publish.topic, &publish.payload).await;
                        self.publish_state(&client).await?;
                    }
                    _ => {}
                },
                _ = ticker.tick() => {
                    self.publish_state(&client).await?;
                }
            }
        }
    }

    async fn handle_command(&self, topic: &str, payload: &[u8]) {
        let payload = String::from_utf8_lossy(payload);
        let command = payload.trim();
        let prefix = &self.config.topic_prefix;
        let (name, value) = match topic
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            Some(name) => (name, command),
            None => {
                warn!(%topic, "MQTT command outside the camera's topic prefix");
                return;
            }
        };
        match name {
            "ptz/set" => {
                self.cancel_hold();
                let body =
                    format!(r#"{{"direction":"{value}","degrees":30,"speed":{COMMAND_SPEED}}}"#);
                self.report("ptz/set", ptz_move(&self.control, body.as_bytes()).await);
            }
            "ptz/hold" => match value {
                "stop" => {
                    self.cancel_hold();
                    self.report("ptz/hold", ptz_stop(&self.control).await);
                }
                direction @ ("left" | "right" | "up" | "down") => {
                    self.start_hold(direction.to_string()).await;
                }
                other => warn!(%other, "ptz/hold takes left, right, up, down or stop"),
            },
            "ptz/move" => match value {
                "stop" | "off" | "" => {
                    self.cancel_hold();
                    self.report("ptz/move", ptz_stop(&self.control).await);
                }
                direction @ ("left" | "right" | "up" | "down") => {
                    self.start_hold(direction.to_string()).await;
                }
                other => warn!(%other, "ptz/move takes left, right, up, down or stop"),
            },
            "ptz/calibrate" => {
                self.cancel_hold();
                self.report("ptz/calibrate", ptz_center(&self.control).await);
            }
            "ptz/center" => {
                self.cancel_hold();
                self.report("ptz/center", ptz_center(&self.control).await);
            }
            "night_mode/set" => {
                let mode = match value {
                    "auto" => 0,
                    "night" => 1,
                    "day" => 2,
                    other => match other.parse::<u8>() {
                        Ok(mode @ 0..=2) => mode,
                        _ => {
                            warn!(%other, "night_mode/set takes auto, night, day, 0, 1 or 2");
                            return;
                        }
                    },
                };
                let body = format!(r#"{{"mode":{mode}}}"#);
                self.report(
                    "night_mode/set",
                    night_mode(&self.control, body.as_bytes()).await,
                );
            }
            "talkback/set" => match value.to_ascii_uppercase().as_str() {
                "ON" | "START" => self.report("talkback/set", talkback_start(&self.control).await),
                "OFF" | "STOP" => self.report("talkback/set", talkback_stop(&self.control).await),
                other => warn!(%other, "talkback/set takes ON or OFF"),
            },
            other => warn!(%other, "unrecognised MQTT command topic"),
        }
    }

    /// Rotate while the button is held: one continuous motor command, ended by
    /// `ptz/hold stop`, any other movement command, or [`HOLD_TIMEOUT`] as a safety
    /// net in case the release message never arrives.
    async fn start_hold(&self, direction: String) {
        let generation = self.holding.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let body = format!(r#"{{"direction":"{direction}","speed":{COMMAND_SPEED}}}"#);
        let (response, motion) = ptz_rotate_owned(&self.control, body.as_bytes()).await;
        self.report("ptz/hold", response);
        let Some(motion) = motion else {
            return;
        };
        let mut shutdown = self.control.shutdown();
        let holding = Arc::clone(&self.holding);
        tokio::spawn(async move {
            let deadline = Instant::now() + HOLD_TIMEOUT;
            while holding.load(Ordering::Acquire) == generation && Instant::now() < deadline {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => break,
                    _ = sleep(HOLD_STEP_INTERVAL) => {},
                }
            }
            // An old release/timeout never stops a newer HTTP or MQTT move.
            let _ = tokio::task::spawn_blocking(move || motion.stop()).await;
        });
    }

    fn cancel_hold(&self) {
        self.holding.fetch_add(1, Ordering::AcqRel);
    }

    /// Log what a command answered; the HTTP status line is enough to tell success.
    fn report(&self, topic: &str, response: Vec<u8>) {
        let line = String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        if line.contains(" 200 ") {
            info!(topic, "MQTT command applied");
        } else {
            warn!(topic, response = %line, "MQTT command rejected");
        }
    }

    /// Publish availability, human status, state topics and telemetry.
    ///
    /// Telemetry is the same document `/status` serves, so the card and the Home
    /// Assistant integration cannot be told different things about one camera.
    async fn publish_state(&self, client: &AsyncClient) -> Result<(), rumqttc::ClientError> {
        let prefix = &self.config.topic_prefix;
        let telemetry = response_body(&status_response(&self.control).await).to_string();
        let streaming = telemetry.contains(r#""status":"streaming""#);
        client
            .publish(
                format!("{prefix}/status"),
                QoS::AtLeastOnce,
                true,
                if streaming { "streaming" } else { "starting" },
            )
            .await?;

        let talkback = response_body(&talkback_status_response(&self.control)).to_string();
        client
            .publish(
                format!("{prefix}/talkback/state"),
                QoS::AtLeastOnce,
                true,
                if talkback.contains(r#""talking":true"#) {
                    "ON"
                } else {
                    "OFF"
                },
            )
            .await?;

        // Reported from the same document, so the state topic cannot disagree with
        // the telemetry about what the camera was last told.
        let mode = telemetry
            .split(r#""mode":""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or("auto")
            .to_string();
        client
            .publish(
                format!("{prefix}/night_mode/state"),
                QoS::AtLeastOnce,
                true,
                mode,
            )
            .await?;

        // What the infrared is doing, as opposed to what it was told to do: in
        // automatic mode the mode above stays `auto` all night, so a switch
        // built on it would never light up.
        let night_vision = telemetry
            .split(r#""night_vision":{"active":"#)
            .nth(1)
            .is_some_and(|rest| rest.starts_with("true"));
        client
            .publish(
                format!("{prefix}/night_vision/state"),
                QoS::AtLeastOnce,
                true,
                if night_vision { "ON" } else { "OFF" },
            )
            .await?;

        client
            .publish(
                format!("{prefix}/status/json"),
                QoS::AtLeastOnce,
                true,
                telemetry,
            )
            .await?;
        Ok(())
    }

    /// Publish Home Assistant discovery documents, one per entity.
    async fn publish_discovery(&self, client: &AsyncClient) -> Result<(), rumqttc::ClientError> {
        let prefix = &self.config.topic_prefix;
        let discovery = &self.config.discovery_prefix;
        let device = format!(
            r#""device":{{"identifiers":["{prefix}"],"name":"Eufy C220","manufacturer":"eufy","model":"C220"}}"#
        );
        let entities = [
            (
                "binary_sensor",
                "status",
                format!(
                    r#"{{"unique_id":"{prefix}_status","name":"Camera","state_topic":"{prefix}/availability","payload_on":"online","payload_off":"offline","device_class":"connectivity",{device}}}"#
                ),
            ),
            (
                "switch",
                "talkback",
                format!(
                    r#"{{"unique_id":"{prefix}_talkback","name":"Talkback","state_topic":"{prefix}/talkback/state","command_topic":"{prefix}/talkback/set","payload_on":"ON","payload_off":"OFF",{device}}}"#
                ),
            ),
            // Home Assistant defaults `state_on`/`state_off` to the command payloads, so
            // this switch would look for "night"/"day" on the state topic, ignore the
            // ON/OFF the daemon publishes there, and sit at "unknown" forever. Spell out
            // that the state payloads are ON/OFF while the commands stay night/day.
            (
                "switch",
                "infrared",
                format!(
                    r#"{{"unique_id":"{prefix}_infrared","name":"Infrared","state_topic":"{prefix}/night_vision/state","command_topic":"{prefix}/night_mode/set","payload_on":"night","payload_off":"day","state_on":"ON","state_off":"OFF",{device}}}"#
                ),
            ),
            (
                "select",
                "night_mode",
                format!(
                    r#"{{"unique_id":"{prefix}_night_mode","name":"Night mode","state_topic":"{prefix}/night_mode/state","command_topic":"{prefix}/night_mode/set","options":["auto","night","day"],{device}}}"#
                ),
            ),
            (
                "button",
                "ptz_left",
                format!(
                    r#"{{"unique_id":"{prefix}_ptz_left","name":"PTZ left","command_topic":"{prefix}/ptz/set","payload_press":"left",{device}}}"#
                ),
            ),
            (
                "button",
                "ptz_right",
                format!(
                    r#"{{"unique_id":"{prefix}_ptz_right","name":"PTZ right","command_topic":"{prefix}/ptz/set","payload_press":"right",{device}}}"#
                ),
            ),
            (
                "button",
                "ptz_up",
                format!(
                    r#"{{"unique_id":"{prefix}_ptz_up","name":"PTZ up","command_topic":"{prefix}/ptz/set","payload_press":"up",{device}}}"#
                ),
            ),
            (
                "button",
                "ptz_down",
                format!(
                    r#"{{"unique_id":"{prefix}_ptz_down","name":"PTZ down","command_topic":"{prefix}/ptz/set","payload_press":"down",{device}}}"#
                ),
            ),
            (
                "button",
                "ptz_center",
                format!(
                    r#"{{"unique_id":"{prefix}_ptz_center","name":"PTZ center","command_topic":"{prefix}/ptz/center","payload_press":"center",{device}}}"#
                ),
            ),
        ];
        // `unique_id` is not optional in practice: Home Assistant registers an entity
        // only when it has one, and a device is formed from registered entities, so
        // without it the entities exist but no camera ever appears under the MQTT
        // integration. The id has to stay stable across restarts, because it is what
        // the registry keys on.
        for (component, object, payload) in entities {
            client
                .publish(
                    format!("{discovery}/{component}/{prefix}/{object}/config"),
                    QoS::AtLeastOnce,
                    true,
                    payload,
                )
                .await?;
        }
        info!(prefix = %discovery, "published Home Assistant discovery");
        Ok(())
    }
}
