//! Loopback ports announced by pane output, and whether they are listening.
//!
//! Pane output only nominates a port. A background probe connects to it on this host, and only
//! listening ports are published to clients, which may forward them to another machine.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::App;
use crate::events::AppEvent;
use crate::layout::PaneId;

/// A port that never starts listening is forgotten this long after its last announcement.
const CANDIDATE_TTL: Duration = Duration::from_secs(60);
const CANDIDATE_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const LISTENING_PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// Consecutive failed probes before a listening port is withdrawn.
const CLOSED_AFTER_MISSES: u8 = 2;
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// Bounds probe work when output announces many ports.
const MAX_PORTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnnouncedPort {
    pane_id: PaneId,
    announced_at: Instant,
    listening: bool,
    misses: u8,
}

#[derive(Debug, Default)]
pub(crate) struct AnnouncedPorts {
    ports: BTreeMap<u16, AnnouncedPort>,
    probe_in_flight: bool,
    next_probe_at: Option<Instant>,
    /// Probing paused while no client subscribed, so listening ports are unconfirmed.
    unverified: bool,
}

impl AnnouncedPorts {
    pub(crate) fn announce(&mut self, pane_id: PaneId, ports: &[u16], now: Instant) {
        for &port in ports {
            let entry = self.ports.entry(port).or_insert(AnnouncedPort {
                pane_id,
                announced_at: now,
                listening: false,
                misses: 0,
            });
            entry.pane_id = pane_id;
            entry.announced_at = now;
        }
        while self.ports.len() > MAX_PORTS {
            let oldest = self
                .ports
                .iter()
                .min_by_key(|(_, port)| (port.listening, port.announced_at))
                .map(|(port, _)| *port);
            if let Some(oldest) = oldest {
                self.ports.remove(&oldest);
            }
        }
        if !ports.is_empty() {
            self.next_probe_at = Some(now);
        }
    }

    /// Resumes probing after a pause. Returns whether the list must be probed before it is
    /// published, because ports may have closed while nothing checked them.
    pub(crate) fn resume(&mut self, now: Instant) -> bool {
        // Candidates are not published, so only listening ports need confirmation.
        if !self.ports.values().any(|port| port.listening) {
            return false;
        }
        self.unverified = true;
        if !self.probe_in_flight {
            self.next_probe_at = Some(now);
        }
        true
    }

    pub(crate) fn probe_deadline(&self) -> Option<Instant> {
        if self.probe_in_flight || self.ports.is_empty() {
            return None;
        }
        self.next_probe_at
    }

    /// Marks a probe as started and returns the ports it must check.
    pub(crate) fn begin_probe(&mut self) -> Vec<u16> {
        self.probe_in_flight = true;
        self.next_probe_at = None;
        self.ports.keys().copied().collect()
    }

    /// Applies probe results. Returns whether the set of listening ports changed.
    pub(crate) fn finish_probe(&mut self, results: &[(u16, bool)], now: Instant) -> bool {
        self.probe_in_flight = false;
        let closed_after = if std::mem::take(&mut self.unverified) {
            1
        } else {
            CLOSED_AFTER_MISSES
        };
        let before = self.listening_ports();
        for &(port, listening) in results {
            let Some(entry) = self.ports.get_mut(&port) else {
                continue;
            };
            if listening {
                entry.listening = true;
                entry.misses = 0;
            } else if entry.listening {
                entry.misses = entry.misses.saturating_add(1);
                if entry.misses >= closed_after {
                    entry.listening = false;
                }
            }
        }
        // A port that closed must be announced again before it is published again.
        self.ports.retain(|_, port| {
            port.listening
                || port.misses == 0
                    && now.saturating_duration_since(port.announced_at) < CANDIDATE_TTL
        });
        let waiting_for_candidate = self.ports.values().any(|port| !port.listening);
        self.next_probe_at = (!self.ports.is_empty()).then(|| {
            now + if waiting_for_candidate {
                CANDIDATE_PROBE_INTERVAL
            } else {
                LISTENING_PROBE_INTERVAL
            }
        });
        before != self.listening_ports()
    }

    fn listening_ports(&self) -> Vec<u16> {
        self.ports
            .iter()
            .filter(|(_, port)| port.listening)
            .map(|(port, _)| *port)
            .collect()
    }

    pub(crate) fn listening(&self) -> impl Iterator<Item = (u16, PaneId)> + '_ {
        self.ports
            .iter()
            .filter(|(_, port)| port.listening)
            .map(|(port, entry)| (*port, entry.pane_id))
    }
}

/// Returns whether anything on this host accepts TCP connections on `port` at a loopback
/// address. Servers bound to the wildcard address also accept loopback connections.
pub(crate) fn port_is_listening(port: u16) -> bool {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|address| TcpStream::connect_timeout(address, PROBE_TIMEOUT).is_ok())
}

impl App {
    pub(crate) fn start_port_probe_if_due(&mut self, now: Instant) {
        if self
            .announced_ports
            .probe_deadline()
            .is_none_or(|deadline| now < deadline)
        {
            return;
        }
        let ports = self.announced_ports.begin_probe();
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let results = ports
                .into_iter()
                .map(|port| (port, port_is_listening(port)))
                .collect();
            let _ = event_tx.blocking_send(AppEvent::PortProbeFinished { results });
        });
    }

    pub(crate) fn port_announcements(
        &self,
        boot_id: &str,
    ) -> crate::protocol::endpoint::EndpointPortAnnouncements {
        crate::protocol::endpoint::EndpointPortAnnouncements {
            boot_id: boot_id.to_owned(),
            ports: self
                .announced_ports
                .listening()
                .map(
                    |(port, pane_id)| crate::protocol::endpoint::EndpointAnnouncedPort {
                        port,
                        pane_id: self
                            .find_pane(pane_id)
                            .and_then(|(ws_idx, _)| self.public_pane_id(ws_idx, pane_id)),
                    },
                )
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> PaneId {
        PaneId::from_raw(1)
    }

    #[test]
    fn announcing_schedules_an_immediate_probe() {
        let mut ports = AnnouncedPorts::default();
        let now = Instant::now();
        assert_eq!(ports.probe_deadline(), None);
        ports.announce(pane(), &[5173], now);
        assert_eq!(ports.probe_deadline(), Some(now));
        assert_eq!(ports.begin_probe(), vec![5173]);
        assert_eq!(ports.probe_deadline(), None, "no overlapping probes");
    }

    #[test]
    fn only_listening_ports_are_published() {
        let mut ports = AnnouncedPorts::default();
        let now = Instant::now();
        ports.announce(pane(), &[3000, 4000], now);
        ports.begin_probe();
        assert!(ports.finish_probe(&[(3000, true), (4000, false)], now));
        assert_eq!(ports.listening().collect::<Vec<_>>(), vec![(3000, pane())]);
        assert_eq!(
            ports.probe_deadline(),
            Some(now + CANDIDATE_PROBE_INTERVAL),
            "a candidate is probed again soon"
        );
        ports.begin_probe();
        assert!(!ports.finish_probe(&[(3000, true), (4000, false)], now));
    }

    #[test]
    fn a_port_closes_after_consecutive_misses() {
        let mut ports = AnnouncedPorts::default();
        let now = Instant::now();
        ports.announce(pane(), &[3000], now);
        ports.begin_probe();
        ports.finish_probe(&[(3000, true)], now);
        assert_eq!(ports.probe_deadline(), Some(now + LISTENING_PROBE_INTERVAL));
        ports.begin_probe();
        assert!(
            !ports.finish_probe(&[(3000, false)], now),
            "one miss is tolerated"
        );
        ports.begin_probe();
        assert!(ports.finish_probe(&[(3000, false)], now));
        assert_eq!(ports.listening().count(), 0);
        assert_eq!(ports.probe_deadline(), None, "closed ports are forgotten");
    }

    #[test]
    fn resumed_probing_closes_a_port_after_one_miss() {
        let mut ports = AnnouncedPorts::default();
        let now = Instant::now();
        assert!(!ports.resume(now), "nothing to verify");
        ports.announce(pane(), &[3000], now);
        ports.begin_probe();
        ports.finish_probe(&[(3000, true)], now);
        assert!(ports.resume(now));
        assert_eq!(ports.probe_deadline(), Some(now));
        ports.begin_probe();
        assert!(ports.finish_probe(&[(3000, false)], now));
        assert_eq!(ports.listening().count(), 0);
    }

    #[test]
    fn candidates_expire_when_they_never_listen() {
        let mut ports = AnnouncedPorts::default();
        let now = Instant::now();
        ports.announce(pane(), &[3000], now);
        ports.begin_probe();
        ports.finish_probe(&[(3000, false)], now + CANDIDATE_TTL);
        assert_eq!(ports.probe_deadline(), None);
    }

    #[test]
    fn a_late_announcement_keeps_a_candidate_alive() {
        let mut ports = AnnouncedPorts::default();
        let now = Instant::now();
        ports.announce(pane(), &[3000], now);
        ports.announce(pane(), &[3000], now + CANDIDATE_TTL);
        ports.begin_probe();
        ports.finish_probe(&[(3000, false)], now + CANDIDATE_TTL);
        assert!(ports.probe_deadline().is_some());
    }

    #[test]
    fn the_port_count_is_bounded() {
        let mut ports = AnnouncedPorts::default();
        let now = Instant::now();
        let many = (10_000..10_000 + MAX_PORTS as u16 + 10).collect::<Vec<_>>();
        ports.announce(pane(), &many, now);
        assert_eq!(ports.begin_probe().len(), MAX_PORTS);
    }

    #[test]
    fn probe_detects_a_real_listener() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_is_listening(port));
        drop(listener);
        assert!(!port_is_listening(port));
    }
}
