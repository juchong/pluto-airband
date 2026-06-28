//! MQTT publisher for Home Assistant.
//!
//! Ponytail-minimal: one background thread reuses the existing [`Metrics`]
//! snapshot (no new sampling path), publishes a single **retained JSON state
//! topic** every interval, and emits **Home Assistant MQTT discovery** configs
//! once per connect so entities auto-appear (no manual HA YAML). A **Last Will**
//! flips availability to `offline` the instant the reader dies, so the whole
//! dashboard greys out on a crash/Pi-down for free.
//!
//! Each HA sensor maps to the one state topic via a `value_template`, so a
//! single publish updates every entity.

use crate::metrics::Metrics;
use rumqttc::{Client, Event, LastWill, MqttOptions, Packet, QoS};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Clone)]
pub struct MqttConfig {
    pub broker: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
    /// Base topic prefix and HA node id, e.g. `pluto-airband`.
    pub prefix: String,
    pub discovery_prefix: String,
    pub interval: Duration,
    pub per_channel: bool,
    pub n_channels: usize,
}

impl MqttConfig {
    fn state_topic(&self) -> String {
        format!("{}/state", self.prefix)
    }
    fn availability_topic(&self) -> String {
        format!("{}/availability", self.prefix)
    }
    /// Slugged node id usable in `unique_id`s (HA dislikes dashes there).
    fn node(&self) -> String {
        self.prefix.replace(['/', '-'], "_")
    }
}

/// A discovery entity bound to the shared state topic.
struct Entity {
    component: &'static str, // "binary_sensor" | "sensor"
    key: String,             // discovery object id + value_json field path
    name: String,
    value_template: String,
    extra: String, // trailing JSON fields (unit, device_class, payload_on/off…)
}

fn entities(cfg: &MqttConfig) -> Vec<Entity> {
    let mut v = Vec::new();
    let mut binary = |key: &str, name: &str| {
        v.push(Entity {
            component: "binary_sensor",
            key: key.to_string(),
            name: name.to_string(),
            value_template: format!("{{{{ value_json.{key} }}}}"),
            extra: "\"payload_on\":\"True\",\"payload_off\":\"False\"".to_string(),
        });
    };
    binary("system_healthy", "Capture healthy");
    binary("liveatc_healthy", "LiveATC healthy");
    binary("pluto_reachable", "Pluto reachable");
    binary("maia_httpd_up", "maia-httpd up");
    binary("data_flowing", "Data flowing");
    binary("dma_advancing", "Pluto DMA advancing");
    binary("fpga_overflow", "Pluto FPGA overflow");

    let mut sensor = |key: &str, name: &str, extra: &str| {
        v.push(Entity {
            component: "sensor",
            key: key.to_string(),
            name: name.to_string(),
            value_template: format!("{{{{ value_json.{key} }}}}"),
            extra: extra.to_string(),
        });
    };
    sensor(
        "seconds_since_last_sample",
        "Seconds since last sample",
        "\"unit_of_measurement\":\"s\"",
    );
    sensor(
        "uptime_secs",
        "Reader uptime",
        "\"unit_of_measurement\":\"s\",\"device_class\":\"duration\"",
    );
    sensor("active_channels", "Active channels", "");
    sensor("total_drops", "Total dropped samples", "\"state_class\":\"total_increasing\"");
    sensor(
        "total_transmissions",
        "Total transmissions",
        "\"state_class\":\"total_increasing\"",
    );

    if cfg.per_channel {
        for i in 0..cfg.n_channels {
            v.push(Entity {
                component: "binary_sensor",
                key: format!("ch{i}_open"),
                name: format!("Ch {i} squelch open"),
                value_template: format!("{{{{ value_json.channels[{i}].open }}}}"),
                extra: "\"payload_on\":\"True\",\"payload_off\":\"False\"".to_string(),
            });
            v.push(Entity {
                component: "sensor",
                key: format!("ch{i}_carrier_dbc"),
                name: format!("Ch {i} carrier"),
                value_template: format!("{{{{ value_json.channels[{i}].carrier_dbc }}}}"),
                extra: "\"unit_of_measurement\":\"dB\"".to_string(),
            });
        }
    }
    v
}

fn device_block(cfg: &MqttConfig) -> String {
    format!(
        "\"device\":{{\"identifiers\":[\"{node}\"],\"name\":\"Pluto Airband\",\"manufacturer\":\"pluto-airband\",\"model\":\"airband-reader\"}}",
        node = cfg.node()
    )
}

/// Publishes availability=online and all discovery configs (retained). Called on
/// every (re)connect so a broker restart re-seeds discovery.
fn announce(client: &Client, cfg: &MqttConfig, ents: &[Entity]) {
    let _ = client.publish(cfg.availability_topic(), QoS::AtLeastOnce, true, "online");
    let state = cfg.state_topic();
    let avail = cfg.availability_topic();
    let dev = device_block(cfg);
    let node = cfg.node();
    for e in ents {
        let topic = format!(
            "{}/{}/{}/{}/config",
            cfg.discovery_prefix, e.component, node, e.key
        );
        let extra = if e.extra.is_empty() {
            String::new()
        } else {
            format!(",{}", e.extra)
        };
        let payload = format!(
            "{{\"name\":{name:?},\"unique_id\":\"{node}_{key}\",\"state_topic\":\"{state}\",\"availability_topic\":\"{avail}\",\"value_template\":{vt:?}{extra},{dev}}}",
            name = e.name,
            key = e.key,
            vt = e.value_template,
        );
        let _ = client.publish(topic, QoS::AtLeastOnce, true, payload);
    }
}

/// Spawns the MQTT publisher. Returns immediately; failures are logged and the
/// rumqttc event loop reconnects on its own.
pub fn spawn(metrics: Arc<Metrics>, cfg: MqttConfig) {
    thread::spawn(move || {
        let mut opts = MqttOptions::new(cfg.prefix.clone(), cfg.broker.clone(), cfg.port);
        opts.set_keep_alive(Duration::from_secs(15));
        if let (Some(u), Some(p)) = (cfg.user.clone(), cfg.pass.clone()) {
            opts.set_credentials(u, p);
        }
        opts.set_last_will(LastWill::new(
            cfg.availability_topic(),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));

        let (client, mut connection) = Client::new(opts, 32);

        // Drive the event loop; (re)announce on each successful connect.
        let ev_client = client.clone();
        let ev_cfg = cfg.clone();
        thread::spawn(move || {
            let ents = entities(&ev_cfg);
            for ev in connection.iter() {
                match ev {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        eprintln!("mqtt: connected to {}:{}", ev_cfg.broker, ev_cfg.port);
                        announce(&ev_client, &ev_cfg, &ents);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("mqtt: connection error ({e}); reconnecting");
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        });

        // Publish the curated snapshot on the retained state topic.
        let state = cfg.state_topic();
        loop {
            thread::sleep(cfg.interval);
            let _ = client.publish(state.clone(), QoS::AtLeastOnce, true, metrics.status_json());
        }
    });
}
