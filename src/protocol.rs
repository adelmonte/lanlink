//! KDE Connect wire format: newline-delimited JSON packets.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: i64 = 8;
pub const UDP_PORT: u16 = 1716;
pub const TCP_PORTS: std::ops::RangeInclusive<u16> = 1716..=1764;
pub const PAYLOAD_PORTS: std::ops::RangeInclusive<u16> = 1739..=1764;

pub const IDENTITY: &str = "kdeconnect.identity";
pub const PAIR: &str = "kdeconnect.pair";
pub const PING: &str = "kdeconnect.ping";
pub const BATTERY: &str = "kdeconnect.battery";
pub const SHARE: &str = "kdeconnect.share.request";
pub const SHARE_UPDATE: &str = "kdeconnect.share.request.update";
pub const SFTP: &str = "kdeconnect.sftp";
pub const SFTP_REQUEST: &str = "kdeconnect.sftp.request";
pub const MPRIS: &str = "kdeconnect.mpris";
pub const MPRIS_REQUEST: &str = "kdeconnect.mpris.request";
pub const MOUSEPAD: &str = "kdeconnect.mousepad.request";
pub const MOUSEPAD_ECHO: &str = "kdeconnect.mousepad.echo";
pub const MOUSEPAD_KEYBOARD: &str = "kdeconnect.mousepad.keyboardstate";
/// A song the phone recognized (lanlink only): title, artist, album, isrc.
pub const SONG: &str = "lanlink.song";

/// Packets we accept. The phone app hides features we don't list here.
pub const INCOMING: &[&str] = &[PING, BATTERY, SHARE, SHARE_UPDATE, SFTP, MPRIS_REQUEST, MOUSEPAD, SONG];
/// Packets we send.
pub const OUTGOING: &[&str] = &[
    PING, SHARE, SFTP_REQUEST, MPRIS, MOUSEPAD, MOUSEPAD_ECHO, MOUSEPAD_KEYBOARD,
];

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Packet {
    #[serde(default)]
    pub id: Value,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub body: Value,
    #[serde(rename = "payloadSize", default, skip_serializing_if = "Option::is_none")]
    pub payload_size: Option<i64>,
    #[serde(rename = "payloadTransferInfo", default, skip_serializing_if = "Option::is_none")]
    pub payload_info: Option<Value>,
}

impl Packet {
    pub fn new(kind: &str, body: Value) -> Self {
        Packet { id: json!(now_ms()), kind: kind.into(), body, payload_size: None, payload_info: None }
    }

    pub fn to_line(&self) -> Vec<u8> {
        let mut v = serde_json::to_vec(self).expect("packet serializes");
        v.push(b'\n');
        v
    }

    pub fn parse(line: &[u8]) -> Result<Self> {
        serde_json::from_slice(line).context("bad packet")
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.body.get(key).and_then(Value::as_str)
    }

    pub fn bool(&self, key: &str) -> bool {
        self.body.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.body.get(key).and_then(Value::as_f64)
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        self.body.get(key).and_then(Value::as_i64)
    }

    pub fn payload_port(&self) -> Option<u16> {
        self.payload_info.as_ref()?.get("port")?.as_u64().map(|p| p as u16)
    }
}

pub fn identity(device_id: &str, name: &str, kind: &str, tcp_port: Option<u16>) -> Packet {
    let mut body = json!({
        "deviceId": device_id,
        "deviceName": name,
        "deviceType": kind,
        "protocolVersion": PROTOCOL_VERSION,
        "incomingCapabilities": INCOMING,
        "outgoingCapabilities": OUTGOING,
    });
    if let Some(p) = tcp_port {
        body["tcpPort"] = json!(p);
    }
    Packet::new(IDENTITY, body)
}

pub fn valid_device_id(id: &str) -> bool {
    (32..=38).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let p = identity("abcdefabcdefabcdefabcdefabcdefab", "box", "desktop", Some(1716));
        let line = p.to_line();
        assert_eq!(*line.last().unwrap(), b'\n');
        let q = Packet::parse(&line[..line.len() - 1]).unwrap();
        assert_eq!(q.kind, IDENTITY);
        assert_eq!(q.i64("tcpPort"), Some(1716));
        assert!(q.payload_size.is_none());
    }

    #[test]
    fn parses_android_payload() {
        let raw = br#"{"id":1,"type":"kdeconnect.share.request","body":{"filename":"a.jpg"},"payloadSize":42,"payloadTransferInfo":{"port":1739}}"#;
        let p = Packet::parse(raw).unwrap();
        assert_eq!(p.payload_size, Some(42));
        assert_eq!(p.payload_port(), Some(1739));
        assert_eq!(p.str("filename"), Some("a.jpg"));
    }

    #[test]
    fn device_ids() {
        assert!(valid_device_id("0123456789abcdef0123456789abcdef"));
        assert!(!valid_device_id("short"));
        assert!(!valid_device_id("0123456789abcdef0123456789abcde-"));
    }
}
