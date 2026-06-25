//! Multi-stream Icecast feed configuration loaded from a JSON file.
//!
//! A feeds file lists any number of Icecast destinations. Each entry binds one
//! channel index to one mount on one server, so the same channel can fan out to
//! several servers (repeat the `channel`) and every channel can be streamed
//! (one entry each). Example:
//!
//! ```json
//! {
//!   "feeds": [
//!     { "channel": 0, "server": "icecast.example.net", "port": 8000,
//!       "mountpoint": "/KRNT-118p050.mp3", "password": "hackme",
//!       "name": "118.050 AWOS", "genre": "ATC" },
//!     { "channel": 0, "server": "backup.example.net", "port": 8000,
//!       "mountpoint": "/KRNT-118p050.mp3", "password": "hackme2" }
//!   ]
//! }
//! ```

use crate::icecast::{IcecastConfig, TlsMode};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct FeedsFile {
    feeds: Vec<FeedEntry>,
}

#[derive(Deserialize)]
struct FeedEntry {
    channel: usize,
    server: String,
    #[serde(default = "default_port")]
    port: u16,
    mountpoint: String,
    #[serde(default = "default_user")]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    genre: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_bitrate")]
    bitrate: u32,
    #[serde(default = "default_samplerate")]
    samplerate: u32,
    #[serde(default = "default_tls")]
    tls: String,
    #[serde(default)]
    tls_insecure: bool,
}

fn default_port() -> u16 {
    8000
}
fn default_user() -> String {
    "source".to_string()
}
fn default_bitrate() -> u32 {
    16
}
fn default_samplerate() -> u32 {
    22050
}
fn default_tls() -> String {
    "disabled".to_string()
}

/// Parses a feeds JSON document into per-channel `IcecastConfig`s.
///
/// `in_rate` is the source (channel) audio rate and `n_channels` bounds the
/// channel index. Returns the configs in file order.
fn parse(json: &str, in_rate: u32, n_channels: usize) -> Result<Vec<IcecastConfig>> {
    let file: FeedsFile = serde_json::from_str(json).context("parsing feeds JSON")?;
    if file.feeds.is_empty() {
        bail!("feeds file has no entries");
    }
    let mut out = Vec::with_capacity(file.feeds.len());
    for (i, f) in file.feeds.into_iter().enumerate() {
        if f.channel >= n_channels {
            bail!(
                "feeds[{i}]: channel {} out of range (0..{})",
                f.channel,
                n_channels
            );
        }
        let tls = TlsMode::parse(&f.tls).map_err(|e| anyhow::anyhow!("feeds[{i}]: {e}"))?;
        let name = f
            .name
            .unwrap_or_else(|| format!("Pluto airband ch{}", f.channel));
        out.push(IcecastConfig {
            host: f.server,
            port: f.port,
            mount: f.mountpoint,
            user: f.username,
            password: f.password,
            bitrate: f.bitrate,
            out_rate: f.samplerate,
            in_rate,
            name,
            genre: f.genre,
            description: f.description,
            tls,
            tls_insecure: f.tls_insecure,
            channel: f.channel,
        });
    }
    Ok(out)
}

/// Loads and parses a feeds file from disk.
pub fn load(path: &Path, in_rate: u32, n_channels: usize) -> Result<Vec<IcecastConfig>> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("reading feeds file {path:?}"))?;
    parse(&json, in_rate, n_channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_and_defaults() {
        let json = r#"{ "feeds": [
            { "channel": 0, "server": "h", "mountpoint": "/m.mp3", "password": "p" }
        ] }"#;
        let v = parse(json, 15625, 21).unwrap();
        assert_eq!(v.len(), 1);
        let c = &v[0];
        assert_eq!(c.channel, 0);
        assert_eq!(c.port, 8000);
        assert_eq!(c.user, "source");
        assert_eq!(c.bitrate, 16);
        assert_eq!(c.out_rate, 22050);
        assert_eq!(c.in_rate, 15625);
        assert_eq!(c.tls, TlsMode::Disabled);
        assert!(!c.tls_insecure);
        assert_eq!(c.name, "Pluto airband ch0");
    }

    #[test]
    fn parses_full_entry_and_fanout() {
        let json = r#"{ "feeds": [
            { "channel": 3, "server": "a", "port": 9343, "mountpoint": "/x.mp3",
              "username": "u", "password": "p", "name": "X", "genre": "ATC",
              "description": "d", "bitrate": 24, "samplerate": 44100,
              "tls": "transport", "tls_insecure": true },
            { "channel": 3, "server": "b", "mountpoint": "/x.mp3", "password": "p2" }
        ] }"#;
        let v = parse(json, 15625, 21).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].channel, 3);
        assert_eq!(v[0].port, 9343);
        assert_eq!(v[0].genre.as_deref(), Some("ATC"));
        assert_eq!(v[0].bitrate, 24);
        assert_eq!(v[0].tls, TlsMode::Transport);
        assert!(v[0].tls_insecure);
        // Second entry fans the same channel out to another server.
        assert_eq!(v[1].channel, 3);
        assert_eq!(v[1].host, "b");
    }

    #[test]
    fn rejects_out_of_range_channel() {
        let json = r#"{ "feeds": [
            { "channel": 99, "server": "h", "mountpoint": "/m.mp3", "password": "p" }
        ] }"#;
        assert!(parse(json, 15625, 21).is_err());
    }

    #[test]
    fn rejects_bad_tls() {
        let json = r#"{ "feeds": [
            { "channel": 0, "server": "h", "mountpoint": "/m.mp3", "password": "p", "tls": "ssl" }
        ] }"#;
        assert!(parse(json, 15625, 21).is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse(r#"{ "feeds": [] }"#, 15625, 21).is_err());
    }
}
