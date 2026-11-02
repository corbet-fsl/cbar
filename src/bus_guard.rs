//! Session-bus guard: the bar exits with [`ExitCode::BusLost`] when its D-Bus connections are unusable, so the service manager
//! (`Restart=on-failure`) starts a fresh one.
//!
//! Why: on 2026-10-30 one of the bar's connections stopped reading its socket. dbus-broker queued every signal for it
//! (263 MiB in under a week) against the per-user byte quota, hit the quota, and dropped the bar's connection and a dozen
//! more of the session (portals, the secret service). The bar kept running, never noticed, and the quota stayed full: every
//! later `systemctl --user daemon-reload` failed ("Got disconnect on API bus") and with it every home-manager activation.
//!
//! The guard wakes every [`PROBE_INTERVAL`] and checks two things through one long-lived connection of its own:
//! * the bus answers: `GetStats` (dbus-broker's accounting call) or, where the bus has none, nothing more than the call
//!   succeeding is required; a failed connect or call drops the connection so the next probe also proves a fresh connect;
//! * none of this process's connections has more than [`BACKLOG_LIMIT`] bytes queued at the broker, i.e. none has stopped
//!   reading. Normal bars queue nothing; the stalled connection of 2026-10-30 was far above this days before the quota ran out.
//!
//! [`FAILURES_TO_EXIT`] unhealthy observations in a row end the process. One healthy observation resets the count, so a
//! restarted compositor or a short bus outage does not restart the bar.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use tracing::{error, warn};
use zbus::zvariant::{OwnedValue, Value};

use crate::error::ExitCode;

/// Time between two probes.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// A probe that takes longer counts as unhealthy.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Unhealthy observations in a row that end the process.
pub const FAILURES_TO_EXIT: u32 = 3;
/// Bytes queued at the broker for ONE connection of this process above which the connection counts as not reading.
pub const BACKLOG_LIMIT: u32 = 16 * 1024 * 1024;

const PEER_ACCOUNTING: &str = "org.bus1.DBus.Debug.Stats.PeerAccounting";

#[derive(Debug, PartialEq, Eq)]
pub enum Observation {
    Healthy,
    Unhealthy(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Healthy,
    /// Unhealthy, but not for long enough yet: the number of unhealthy observations in a row.
    Degraded(u32),
    /// Unhealthy for [`FAILURES_TO_EXIT`] observations in a row: the reason of the last one.
    Lost(String),
}

/// Counts consecutive unhealthy observations.
#[derive(Debug)]
pub struct Guard {
    limit: u32,
    failures: u32,
}

impl Guard {
    pub fn new(limit: u32) -> Self {
        Self { limit, failures: 0 }
    }

    pub fn observe(&mut self, observation: Observation) -> Verdict {
        match observation {
            Observation::Healthy => {
                self.failures = 0;
                Verdict::Healthy
            }
            Observation::Unhealthy(reason) => {
                self.failures += 1;
                if self.failures >= self.limit {
                    Verdict::Lost(reason)
                } else {
                    Verdict::Degraded(self.failures)
                }
            }
        }
    }
}

/// One health check of the bus.
pub trait Probe {
    fn check(&mut self) -> impl Future<Output = Observation> + Send;
}

/// Runs `probe` every `interval` until [`FAILURES_TO_EXIT`]-style unhealthy observations (`limit`) in a row; returns the last reason.
pub async fn watch<P: Probe>(
    probe: &mut P,
    interval: Duration,
    timeout: Duration,
    limit: u32,
) -> String {
    let mut guard = Guard::new(limit);
    loop {
        tokio::time::sleep(interval).await;
        let observation = match tokio::time::timeout(timeout, probe.check()).await {
            Ok(observation) => observation,
            Err(_) => {
                Observation::Unhealthy(format!("the bus probe did not finish within {timeout:?}"))
            }
        };
        match guard.observe(observation) {
            Verdict::Healthy => {}
            Verdict::Degraded(n) => {
                warn!("session bus unhealthy ({n} of {limit} observations in a row)")
            }
            Verdict::Lost(reason) => return reason,
        }
    }
}

/// Ends the process the way the service manager restarts it.
pub fn exit_lost(reason: &str) -> ! {
    error!("session bus unusable ({reason}); exiting so that the service manager restarts the bar");
    std::process::exit(ExitCode::BusLost as i32)
}

/// Starts the guard on the Tokio runtime.
pub fn spawn() {
    crate::spawn(async {
        let mut probe = BusProbe::new(std::process::id());
        let reason = watch(&mut probe, PROBE_INTERVAL, PROBE_TIMEOUT, FAILURES_TO_EXIT).await;
        exit_lost(&reason);
    });
}

/// The probe on the real session bus.
pub struct BusProbe {
    connection: Option<zbus::Connection>,
    pid: u32,
}

impl BusProbe {
    pub fn new(pid: u32) -> Self {
        Self {
            connection: None,
            pid,
        }
    }

    async fn connection(&mut self) -> Result<zbus::Connection, String> {
        if let Some(connection) = &self.connection {
            return Ok(connection.clone());
        }
        let connection = zbus::Connection::session()
            .await
            .map_err(|e| format!("cannot connect to the session bus: {e}"))?;
        self.connection = Some(connection.clone());
        Ok(connection)
    }
}

impl Probe for BusProbe {
    async fn check(&mut self) -> Observation {
        let connection = match self.connection().await {
            Ok(connection) => connection,
            Err(reason) => return Observation::Unhealthy(reason),
        };
        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Debug.Stats"),
                "GetStats",
                &(),
            )
            .await;
        match reply {
            Ok(message) => match message.body().deserialize::<HashMap<String, OwnedValue>>() {
                Ok(stats) => match largest_backlog(&stats, self.pid) {
                    Some((name, bytes)) if bytes > BACKLOG_LIMIT => {
                        Observation::Unhealthy(format!(
                            "the connection {name} of this process has {bytes} bytes queued at the broker and is not reading"
                        ))
                    }
                    _ => Observation::Healthy,
                },
                Err(e) => {
                    // an answer that cannot be read is no proof of a broken bus
                    warn!("unreadable bus statistics: {e}");
                    Observation::Healthy
                }
            },
            // a bus without the statistics interface (dbus-daemon) answers, so it is alive
            Err(zbus::Error::MethodError(..)) => Observation::Healthy,
            Err(e) => {
                self.connection = None;
                Observation::Unhealthy(format!("the bus call failed: {e}"))
            }
        }
    }
}

/// The connection of process `pid` with the most bytes queued at the broker: (unique name, bytes).
///
/// Reads dbus-broker's `PeerAccounting`, `a(sa{sv}a{su})`: per connection its unique name, its credentials (`ProcessID`)
/// and its accounting (`OutgoingBytes`, what the broker holds for it).
pub fn largest_backlog(stats: &HashMap<String, OwnedValue>, pid: u32) -> Option<(String, u32)> {
    let Value::Array(peers) = unwrap_variant(stats.get(PEER_ACCOUNTING)?) else {
        return None;
    };
    let mut largest: Option<(String, u32)> = None;
    for peer in peers.iter() {
        let Value::Structure(peer) = unwrap_variant(peer) else {
            continue;
        };
        let [Value::Str(name), credentials, accounting] = peer.fields() else {
            continue;
        };
        let (Value::Dict(credentials), Value::Dict(accounting)) =
            (unwrap_variant(credentials), unwrap_variant(accounting))
        else {
            continue;
        };
        let owner = credentials.iter().find_map(|(key, value)| {
            match (unwrap_variant(key), unwrap_variant(value)) {
                (Value::Str(key), Value::U32(pid)) if key.as_str() == "ProcessID" => Some(*pid),
                _ => None,
            }
        });
        if owner != Some(pid) {
            continue;
        }
        let queued = accounting.iter().find_map(|(key, value)| {
            match (unwrap_variant(key), unwrap_variant(value)) {
                (Value::Str(key), Value::U32(bytes)) if key.as_str() == "OutgoingBytes" => {
                    Some(*bytes)
                }
                _ => None,
            }
        });
        if let Some(bytes) = queued
            && largest.as_ref().is_none_or(|(_, most)| bytes > *most)
        {
            largest = Some((name.to_string(), bytes));
        }
    }
    largest
}

/// Strips the variant wrappers (`v`) that dictionary values of type `a{sv}` carry.
fn unwrap_variant<'a>(mut value: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = value {
        value = inner;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use zbus::zvariant::{Array, Dict, Signature, StructureBuilder};

    fn unhealthy(reason: &str) -> Observation {
        Observation::Unhealthy(reason.to_string())
    }

    #[test]
    fn exits_only_after_the_limit_in_a_row() {
        let mut guard = Guard::new(3);
        assert_eq!(guard.observe(unhealthy("a")), Verdict::Degraded(1));
        assert_eq!(guard.observe(unhealthy("b")), Verdict::Degraded(2));
        assert_eq!(guard.observe(unhealthy("c")), Verdict::Lost("c".into()));
    }

    #[test]
    fn a_healthy_observation_resets_the_count() {
        let mut guard = Guard::new(3);
        guard.observe(unhealthy("a"));
        guard.observe(unhealthy("b"));
        assert_eq!(guard.observe(Observation::Healthy), Verdict::Healthy);
        assert_eq!(guard.observe(unhealthy("c")), Verdict::Degraded(1));
        assert_eq!(guard.observe(unhealthy("d")), Verdict::Degraded(2));
    }

    /// dbus-broker's `PeerAccounting` entry `(sa{sv}a{su})`: unique name, credentials, accounting.
    fn peer(name: &str, pid: u32, outgoing: u32) -> Value<'static> {
        let mut credentials = Dict::new(&Signature::Str, &Signature::Variant);
        credentials
            .add("ProcessID", Value::new(pid))
            .expect("a well-formed model value");
        credentials
            .add("UnixUserID", Value::new(1000u32))
            .expect("a well-formed model value");
        let mut accounting = Dict::new(&Signature::Str, &Signature::U32);
        accounting
            .add("Matches", 3u32)
            .expect("a well-formed model value");
        accounting
            .add("OutgoingBytes", outgoing)
            .expect("a well-formed model value");
        Value::Structure(
            StructureBuilder::new()
                .append_field(Value::from(name.to_string()))
                .append_field(Value::Dict(credentials))
                .append_field(Value::Dict(accounting))
                .build()
                .expect("a well-formed model value"),
        )
    }

    fn stats(peers: Vec<Value<'static>>) -> HashMap<String, OwnedValue> {
        let signature = Signature::structure([
            Signature::Str,
            Signature::Dict {
                key: Signature::Str.into(),
                value: Signature::Variant.into(),
            },
            Signature::Dict {
                key: Signature::Str.into(),
                value: Signature::U32.into(),
            },
        ]);
        let mut array = Array::new(&signature);
        for peer in peers {
            array.append(peer).expect("a well-formed model value");
        }
        let mut stats = HashMap::new();
        stats.insert(
            PEER_ACCOUNTING.to_string(),
            Value::Array(array).try_to_owned().expect("test setup"),
        );
        stats
    }

    #[test]
    fn finds_the_largest_backlog_of_this_process_only() {
        let stats = stats(vec![
            peer(":1.1", 100, 0),
            peer(":1.57", 3834, 275_707_052),
            peer(":1.58", 3834, 1_000),
            peer(":1.99", 4242, 900_000_000),
        ]);
        assert_eq!(
            largest_backlog(&stats, 3834),
            Some((":1.57".to_string(), 275_707_052))
        );
        assert_eq!(largest_backlog(&stats, 100), Some((":1.1".to_string(), 0)));
        assert_eq!(largest_backlog(&stats, 7), None);
    }

    #[test]
    fn a_bus_without_accounting_has_no_backlog() {
        assert_eq!(largest_backlog(&HashMap::new(), 1), None);
    }

    struct Scripted {
        calls: AtomicU32,
        script: Vec<Observation>,
    }

    impl Probe for Scripted {
        async fn check(&mut self) -> Observation {
            let i = self.calls.fetch_add(1, Ordering::SeqCst) as usize;
            match self.script.get(i) {
                Some(Observation::Healthy) => Observation::Healthy,
                Some(Observation::Unhealthy(reason)) => Observation::Unhealthy(reason.clone()),
                None => unhealthy("script ended"),
            }
        }
    }

    #[tokio::test]
    async fn watch_returns_the_reason_after_three_unhealthy_probes() {
        let mut probe = Scripted {
            calls: AtomicU32::new(0),
            script: vec![
                Observation::Healthy,
                unhealthy("x"),
                Observation::Healthy,
                unhealthy("y"),
                unhealthy("z"),
                unhealthy("last"),
            ],
        };
        let reason = watch(
            &mut probe,
            Duration::from_millis(1),
            Duration::from_secs(1),
            3,
        )
        .await;
        assert_eq!(reason, "last");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 6);
    }

    struct Hangs;

    impl Probe for Hangs {
        async fn check(&mut self) -> Observation {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_probe_that_never_answers_is_unhealthy() {
        let reason = watch(
            &mut Hangs,
            Duration::from_millis(1),
            Duration::from_millis(5),
            3,
        )
        .await;
        assert!(reason.contains("did not finish"), "{reason}");
    }

    /// The exit code of the whole path: a re-executed copy of the test binary runs the guard on a bus that always fails.
    #[test]
    fn the_process_exits_with_the_bus_lost_code() {
        if std::env::var_os("CBAR_BUS_GUARD_CHILD").is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("test setup");
            let reason = runtime.block_on(async {
                let mut probe = Scripted {
                    calls: AtomicU32::new(0),
                    script: vec![],
                };
                watch(
                    &mut probe,
                    Duration::from_millis(1),
                    Duration::from_secs(1),
                    FAILURES_TO_EXIT,
                )
                .await
            });
            exit_lost(&reason);
        }
        let status = std::process::Command::new(std::env::current_exe().expect("test setup"))
            .args([
                "--exact",
                "bus_guard::tests::the_process_exits_with_the_bus_lost_code",
                "--nocapture",
            ])
            .env("CBAR_BUS_GUARD_CHILD", "1")
            .status()
            .expect("test setup");
        assert_eq!(status.code(), Some(ExitCode::BusLost as i32));
    }

    /// Against the real session bus of a dbus-broker session: `cargo test -- --ignored the_real_bus`.
    #[tokio::test]
    #[ignore = "requires a dbus-broker session bus"]
    async fn the_real_bus_is_healthy() {
        let mut probe = BusProbe::new(std::process::id());
        assert_eq!(probe.check().await, Observation::Healthy);
        // a second probe reuses the connection
        assert_eq!(probe.check().await, Observation::Healthy);

        // the real wire format parses: this process owns at least the connection of the probe, and nothing is queued for it
        let connection = zbus::Connection::session().await.expect("test setup");
        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Debug.Stats"),
                "GetStats",
                &(),
            )
            .await
            .expect("test setup");
        let stats: HashMap<String, OwnedValue> = reply.body().deserialize().expect("test setup");
        let (_, queued) = largest_backlog(&stats, std::process::id())
            .expect("the broker lists this process's connection");
        assert!(
            queued < BACKLOG_LIMIT,
            "{queued} bytes queued for an idle test connection"
        );
    }
}
