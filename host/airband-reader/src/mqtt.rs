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
//!
//! # Never block on the request queue
//!
//! rumqttc's sync [`Client::publish`] is a *blocking* send into a bounded request
//! queue that only the event loop drains — and the event loop drains nothing
//! while the broker is unreachable. An earlier design published the state
//! snapshot unconditionally (filling the queue during an outage) and ran the
//! re-announce from the event-loop thread itself on `ConnAck`; the announce then
//! blocked on the full queue, the event loop was never polled again, the broker
//! expired the keepalive and published the Last Will, and every HA entity stayed
//! `unavailable` until the reader was restarted. Hence the rules here:
//!
//! - the event-loop thread only sets flags; it never publishes or subscribes;
//! - the publisher skips its tick while disconnected, so the queue stays empty
//!   across an outage;
//! - every send is `try_*` (non-blocking): a full queue drops that state
//!   snapshot (it is a retained snapshot, the next one supersedes it) or re-arms
//!   the announce for the next tick;
//! - the queue capacity is sized from the entity count so a full re-announce
//!   always fits.

use crate::metrics::Metrics;
use rumqttc::{Client, Event, LastWill, MqttOptions, Packet, QoS};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
    /// Home Assistant's birth/last-will topic: HA publishes `online` here when it
    /// (re)starts, which is our cue to re-send discovery + availability in case
    /// the broker lost its retained state.
    fn ha_status_topic(&self) -> String {
        format!("{}/status", self.discovery_prefix)
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
    // Consolidated outage flag first: device_class "problem" so HA shows on = a
    // problem, giving the user one entity to trigger an outage notification on
    // (and it flips to `unavailable` if the reader/Pi dies, via the LWT).
    v.push(Entity {
        component: "binary_sensor",
        key: "outage".to_string(),
        name: "Outage".to_string(),
        value_template: "{{ value_json.outage }}".to_string(),
        extra: "\"payload_on\":\"True\",\"payload_off\":\"False\",\"device_class\":\"problem\"".to_string(),
    });

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

/// Queues availability=online and every discovery config (all retained) with
/// non-blocking sends. Returns `false` if the request queue was full for any of
/// them, so the caller re-arms and retries on its next tick rather than leaving
/// HA with a partial entity set.
fn announce(client: &Client, cfg: &MqttConfig, ents: &[Entity]) -> bool {
    let mut ok = client
        .try_publish(cfg.availability_topic(), QoS::AtLeastOnce, true, "online")
        .is_ok();
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
        ok &= client
            .try_publish(topic, QoS::AtLeastOnce, true, payload)
            .is_ok();
    }
    ok
}

/// Spawns the MQTT publisher. Returns immediately.
///
/// Two threads: a **connection** thread drives rumqttc's event loop (which
/// reconnects on its own after any error) and only records state — `connected`
/// on ConnAck / cleared on error, and `reannounce` on ConnAck or on Home
/// Assistant's birth message. A **publisher** thread wakes every interval and,
/// only while connected, performs any pending announce (subscribe to HA's status
/// topic, availability=online, discovery configs) and then pushes the retained
/// state snapshot — all with non-blocking sends. See the module docs for why
/// the event-loop thread must never publish itself.
pub fn spawn(metrics: Arc<Metrics>, cfg: MqttConfig) {
    let ents = Arc::new(entities(&cfg));
    // Request-queue capacity: a full re-announce (availability + every discovery
    // config + the HA status subscribe) and a state snapshot must fit at once,
    // with headroom, or the non-blocking sends would drop part of the announce.
    // With `--mqtt-per-channel` the entity count scales with the channel count,
    // which is why this is derived rather than a fixed small number.
    let cap = (ents.len() + 2) * 2 + 16;

    // The current live client, or `None` while (re)building. The publisher reads
    // it; the connection thread swaps it whenever it builds a client.
    let slot: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
    let connected = Arc::new(AtomicBool::new(false));
    let reannounce = Arc::new(AtomicBool::new(false));

    // Publisher.
    {
        let slot = Arc::clone(&slot);
        let connected = Arc::clone(&connected);
        let reannounce = Arc::clone(&reannounce);
        let ents = Arc::clone(&ents);
        let cfg = cfg.clone();
        thread::spawn(move || {
            let state = cfg.state_topic();
            let ha_status = cfg.ha_status_topic();
            let ticks = cfg.interval.as_secs().max(1);
            let mut queue_full_logged = false;
            loop {
                // Sleep the interval in 1 s slices so a (re)connect is announced
                // within a second instead of up to a full interval later.
                for _ in 0..ticks {
                    thread::sleep(Duration::from_secs(1));
                    if reannounce.load(Ordering::Relaxed) {
                        break;
                    }
                }
                if !connected.load(Ordering::Relaxed) {
                    continue;
                }
                let Some(client) = slot.lock().unwrap().clone() else {
                    continue;
                };

                if reannounce.swap(false, Ordering::Relaxed) {
                    let sub_ok = client
                        .try_subscribe(ha_status.clone(), QoS::AtLeastOnce)
                        .is_ok();
                    let ann_ok = announce(&client, &cfg, &ents);
                    if !(sub_ok && ann_ok) {
                        eprintln!("mqtt: request queue full while announcing; retrying next tick");
                        reannounce.store(true, Ordering::Relaxed);
                        continue;
                    }
                    eprintln!(
                        "mqtt: announced availability=online + {} discovery configs",
                        ents.len()
                    );
                    queue_full_logged = false;
                }

                match client.try_publish(state.clone(), QoS::AtLeastOnce, true, metrics.status_json()) {
                    Ok(()) => queue_full_logged = false,
                    Err(_) => {
                        if !queue_full_logged {
                            eprintln!("mqtt: request queue full; dropping state snapshot");
                            queue_full_logged = true;
                        }
                    }
                }
            }
        });
    }

    // Connection: build the client, drive the event loop, record state.
    thread::spawn(move || {
        let ha_status = cfg.ha_status_topic();
        loop {
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

            let (client, mut connection) = Client::new(opts, cap);
            *slot.lock().unwrap() = Some(client);
            // Log the first error of an outage, then at most once a minute: the
            // event loop retries every few seconds and a long outage would
            // otherwise produce hundreds of identical lines.
            let mut last_err_log: Option<Instant> = None;

            for ev in connection.iter() {
                match ev {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        eprintln!("mqtt: connected to {}:{}", cfg.broker, cfg.port);
                        last_err_log = None;
                        reannounce.store(true, Ordering::Relaxed);
                        connected.store(true, Ordering::Relaxed);
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        if p.topic == ha_status && &p.payload[..] == b"online" {
                            eprintln!("mqtt: Home Assistant came online; re-announcing");
                            reannounce.store(true, Ordering::Relaxed);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // Any error means rumqttc dropped the network and will
                        // reconnect on the next poll; nothing is queued meanwhile.
                        let was_connected = connected.swap(false, Ordering::Relaxed);
                        let due = last_err_log
                            .is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
                        if was_connected || due {
                            eprintln!("mqtt: connection error ({e}); reconnecting");
                            last_err_log = Some(Instant::now());
                        }
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            }

            // The iterator only ends when every `Client` handle is gone, which
            // cannot happen while the slot holds one; kept as a belt-and-braces
            // rebuild so a future refactor can't silently strand the publisher.
            eprintln!("mqtt: event loop ended; rebuilding client");
            connected.store(false, Ordering::Relaxed);
            *slot.lock().unwrap() = None;
            thread::sleep(Duration::from_secs(2));
        }
    });
}
