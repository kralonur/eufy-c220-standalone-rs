use super::*;

fn envelope(operation: &str, header: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"{SOAP}\" xmlns:tds=\"{DEVICE}\" xmlns:tptz=\"{PTZ}\" xmlns:tt=\"{SCHEMA}\" xmlns:wsse=\"{WSSE}\" xmlns:wsu=\"{WSU}\"><s:Header>{header}</s:Header><s:Body>{operation}</s:Body></s:Envelope>"
    )
}
fn service() -> Service {
    Service::new(
        "fixture".into(),
        Zeroizing::new("fixture-value\n".into()),
        322,
    )
}
fn token(password: &str, nonce: &[u8], stamp: OffsetDateTime) -> String {
    let created = stamp.format(&Rfc3339).unwrap();
    let mut hash = Sha1::new();
    hash.update(nonce);
    hash.update(created.as_bytes());
    hash.update(password.as_bytes());
    format!(
        "<wsse:Security><wsse:UsernameToken><wsse:Username>fixture</wsse:Username><wsse:Password Type=\"{PASSWORD_DIGEST}\">{}</wsse:Password><wsse:Nonce>{}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>",
        STANDARD.encode(hash.finalize()),
        STANDARD.encode(nonce)
    )
}

// WSSE tokens must expire and cannot authorize a second operation; incorrect
// proofs must not poison the replay table for a legitimate retry.
#[test]
fn wsse_requires_fresh_correct_nonreplayed_proof() {
    let service = service();
    let now = OffsetDateTime::now_utc();
    for (password, stamp) in [
        ("wrong", now),
        (
            "fixture-value",
            now - time::Duration::seconds(CLOCK_SKEW_SECONDS + 1),
        ),
    ] {
        let xml = envelope(
            "<tds:GetCapabilities/>",
            &token(password, b"test nonce", stamp),
        );
        assert!(!service.authenticate(&Document::parse(&xml).unwrap()));
    }
    let xml = envelope(
        "<tds:GetCapabilities/>",
        &token("fixture-value", b"test nonce", now),
    );
    let document = Document::parse(&xml).unwrap();
    assert!(service.authenticate(&document));
    assert!(!service.authenticate(&document));
}

// Only validated cardinal movement reaches the driver; NaN/diagonal/zoom and
// oversized timeouts must not become ioctl arguments or indefinite movement.
#[test]
fn cardinal_velocities_and_stop_are_bounded() {
    for (x, y, expected) in [
        ("-0.5", "0", PtzDirection::Left),
        ("0.5", "0", PtzDirection::Right),
        ("0", "0.5", PtzDirection::Up),
        ("0", "-0.5", PtzDirection::Down),
        ("0", "0", PtzDirection::Stop),
    ] {
        let xml = envelope(
            &format!(
                "<tptz:ContinuousMove><tptz:Velocity><tt:PanTilt x=\"{x}\" y=\"{y}\"/></tptz:Velocity></tptz:ContinuousMove>"
            ),
            "",
        );
        let document = Document::parse(&xml).unwrap();
        let action = document
            .descendants()
            .find(|n| n.has_tag_name((PTZ, "ContinuousMove")))
            .unwrap();
        let (direction, _, timeout) = movement(action).unwrap();
        assert_eq!(direction, expected);
        assert_eq!(timeout, MOVE_TIMEOUT);
    }
    for velocity in [
        "<tt:PanTilt x=\"NaN\" y=\"0\"/>",
        "<tt:PanTilt x=\"1\" y=\"1\"/>",
        "<tt:PanTilt x=\"2\" y=\"0\"/>",
        "<tt:PanTilt x=\"1\" y=\"0\"/><tt:Zoom x=\"1\"/>",
    ] {
        let xml = envelope(
            &format!(
                "<tptz:ContinuousMove><tptz:Velocity>{velocity}</tptz:Velocity></tptz:ContinuousMove>"
            ),
            "",
        );
        let document = Document::parse(&xml).unwrap();
        assert!(
            movement(
                document
                    .descendants()
                    .find(|n| n.has_tag_name((PTZ, "ContinuousMove")))
                    .unwrap()
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn discovery_authentication_and_unsupported_faults() {
    let server = ControlServer::start(
        ControlServerConfig::new("127.0.0.1:0".parse().unwrap()),
        MediaEngineSlot::default(),
        None,
    )
    .unwrap();
    let service = service();
    let inner = server.inner();
    let address = server.local_addr();
    let xml = envelope("<tds:GetCapabilities/>", "");
    assert!(
        String::from_utf8(
            service
                .dispatch(&inner, xml.as_bytes(), address, address)
                .await
        )
        .unwrap()
        .contains("ter:NotAuthorized")
    );
    let xml = envelope(
        "<tds:GetCapabilities/>",
        &token("fixture-value", b"capabilities", OffsetDateTime::now_utc()),
    );
    let result = String::from_utf8(
        service
            .dispatch(&inner, xml.as_bytes(), address, address)
            .await,
    )
    .unwrap();
    assert!(result.contains(&format!("https://{address}/onvif/ptz_service")));
    assert!(result.contains("GetCapabilitiesResponse"));
    let xml = envelope(
        "<tptz:AbsoluteMove/>",
        &token("fixture-value", b"unsupported", OffsetDateTime::now_utc()),
    );
    assert!(
        String::from_utf8(
            service
                .dispatch(&inner, xml.as_bytes(), address, address)
                .await
        )
        .unwrap()
        .contains("ter:ActionNotSupported")
    );
}
