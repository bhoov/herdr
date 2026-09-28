//! Forward loopback ports that saved machines announce to loopback ports on this computer.
//!
//! A remote server publishes the ports that its pane output announced and that accept
//! connections (`endpoint.ports.v1`). For each one, this client listens on a local loopback
//! port and carries each accepted connection through `ssh -W localhost:<port>`. The local port
//! is the one remembered from an earlier run, else the remote port number, else a nearby free
//! port. The registry file records the mappings, so a restarted client reuses them and a second
//! client on this computer shows an existing forward instead of opening another one.

mod listener;
mod registry;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::client::endpoint::{ProfileId, SavedSshEndpoint};
use crate::protocol::endpoint::EndpointPortAnnouncements;

use listener::{Listener, TunnelHealth, TunnelSource};
use registry::{Registry, RegistryEntry};

/// Ports below this number are system services, not development servers.
const MIN_FORWARDED_PORT: u16 = 1024;
/// How many following port numbers are tried when the preferred local port is in use.
const NEARBY_PORTS: u16 = 20;
/// How often a forward owned by another client is checked for a takeover.
const SHARED_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortForwardState {
    /// This client listens on the local port.
    Active,
    /// Another client on this computer already forwards this remote port.
    Shared,
    /// The last connection through the tunnel failed.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PortForwardView {
    pub(crate) machine: String,
    pub(crate) remote_port: u16,
    pub(crate) local_port: u16,
    pub(crate) state: PortForwardState,
}

enum ForwardOwner {
    Us(Listener),
    Other { owner_pid: u32 },
}

struct Forward {
    local_port: u16,
    owner: ForwardOwner,
}

struct Profile {
    endpoint: SavedSshEndpoint,
    tunnel: Option<Arc<TunnelSource>>,
}

type TunnelFactory = fn(&SavedSshEndpoint) -> std::io::Result<TunnelSource>;

pub(crate) struct PortForwards {
    registry: Registry,
    /// Mappings from exited clients, used as preferred local ports.
    remembered: Vec<RegistryEntry>,
    profiles: BTreeMap<ProfileId, Profile>,
    forwards: BTreeMap<(ProfileId, u16), Forward>,
    /// Machines whose server reported its ports in this session.
    reported: HashSet<ProfileId>,
    runtime: tokio::runtime::Handle,
    tunnel_factory: TunnelFactory,
    next_shared_check: Option<Instant>,
}

impl PortForwards {
    pub(crate) fn new(runtime: tokio::runtime::Handle) -> Self {
        Self::with_registry(
            Registry::new(Registry::default_path()),
            runtime,
            |endpoint| {
                crate::remote::SavedSshTunnel::new(
                    endpoint.id.as_str(),
                    &endpoint.target,
                    &endpoint.session,
                )
                .map(TunnelSource::Ssh)
            },
        )
    }

    fn with_registry(
        registry: Registry,
        runtime: tokio::runtime::Handle,
        tunnel_factory: TunnelFactory,
    ) -> Self {
        let remembered = registry
            .load()
            .into_iter()
            .filter(|entry| !registry.owner_alive(entry.owner_pid))
            .collect();
        Self {
            registry,
            remembered,
            profiles: BTreeMap::new(),
            forwards: BTreeMap::new(),
            reported: HashSet::new(),
            runtime,
            tunnel_factory,
            next_shared_check: None,
        }
    }

    /// Follows the saved-machine catalog. Forwards of removed or disabled machines close.
    pub(crate) fn set_profiles(&mut self, endpoints: &[SavedSshEndpoint]) -> bool {
        let before = self.views();
        let enabled = endpoints
            .iter()
            .filter(|endpoint| endpoint.enabled)
            .collect::<Vec<_>>();
        self.profiles.retain(|id, profile| {
            enabled.iter().any(|endpoint| {
                &endpoint.id == id
                    && endpoint.target == profile.endpoint.target
                    && endpoint.session == profile.endpoint.session
            })
        });
        for endpoint in enabled {
            self.profiles
                .entry(endpoint.id.clone())
                .and_modify(|profile| profile.endpoint.label = endpoint.label.clone())
                .or_insert_with(|| Profile {
                    endpoint: endpoint.clone(),
                    tunnel: None,
                });
        }
        let profiles = &self.profiles;
        let forwards_before = self.forwards.len();
        self.forwards
            .retain(|(profile_id, _), _| profiles.contains_key(profile_id));
        self.reported
            .retain(|profile_id| profiles.contains_key(profile_id));
        if self.forwards.len() != forwards_before {
            self.persist();
        }
        before != self.views()
    }

    /// Applies a machine's current list of listening ports. Returns whether the view changed.
    pub(crate) fn receive(
        &mut self,
        profile_id: &ProfileId,
        announcements: &EndpointPortAnnouncements,
    ) -> bool {
        if !self.profiles.contains_key(profile_id) {
            return false;
        }
        let before = self.views();
        let wanted = announcements
            .ports
            .iter()
            .map(|announced| announced.port)
            .filter(|port| *port >= MIN_FORWARDED_PORT)
            .collect::<BTreeSet<_>>();
        self.reported.insert(profile_id.clone());
        self.forwards.retain(|(id, remote_port), forward| {
            let keep = id != profile_id || wanted.contains(remote_port);
            if !keep {
                info!(
                    remote_port,
                    local_port = forward.local_port,
                    "closed port forward"
                );
            }
            keep
        });
        for remote_port in wanted {
            if !self
                .forwards
                .contains_key(&(profile_id.clone(), remote_port))
            {
                self.open(profile_id, remote_port);
            }
        }
        // Remembered ports that the machine no longer serves are forgotten.
        self.remembered.retain(|entry| {
            &entry.profile_id != profile_id
                || self
                    .forwards
                    .contains_key(&(entry.profile_id.clone(), entry.remote_port))
        });
        self.persist();
        before != self.views()
    }

    /// Takes over forwards whose owning client exited. Returns whether the view changed.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        if self.next_shared_check.is_none_or(|deadline| now < deadline) {
            return false;
        }
        self.next_shared_check = None;
        let before = self.views();
        let shared = self
            .forwards
            .iter()
            .filter_map(|(key, forward)| match forward.owner {
                ForwardOwner::Other { owner_pid } => Some((key.clone(), owner_pid)),
                ForwardOwner::Us(_) => None,
            })
            .collect::<Vec<_>>();
        for ((profile_id, remote_port), owner_pid) in shared {
            if self.registry.owner_alive(owner_pid)
                && self.live_owner_serves(&profile_id, remote_port)
            {
                continue;
            }
            self.forwards.remove(&(profile_id.clone(), remote_port));
            self.open(&profile_id, remote_port);
        }
        self.persist();
        before != self.views()
    }

    pub(crate) fn views(&self) -> Vec<PortForwardView> {
        let mut views = self
            .forwards
            .iter()
            .filter_map(|((profile_id, remote_port), forward)| {
                let profile = self.profiles.get(profile_id)?;
                Some(PortForwardView {
                    machine: profile.endpoint.label.clone(),
                    remote_port: *remote_port,
                    local_port: forward.local_port,
                    state: match &forward.owner {
                        ForwardOwner::Other { .. } => PortForwardState::Shared,
                        ForwardOwner::Us(listener) if listener.health() == TunnelHealth::Failed => {
                            PortForwardState::Failed
                        }
                        ForwardOwner::Us(_) => PortForwardState::Active,
                    },
                })
            })
            .collect::<Vec<_>>();
        views.sort_by(|left, right| {
            (&left.machine, left.remote_port).cmp(&(&right.machine, right.remote_port))
        });
        views
    }

    fn open(&mut self, profile_id: &ProfileId, remote_port: u16) {
        let foreign = self.registry.live_foreign_entries();
        if let Some(entry) = foreign.iter().find(|entry| {
            &entry.profile_id == profile_id
                && entry.remote_port == remote_port
                && listener::loopback_port_accepts(entry.local_port)
        }) {
            self.forwards.insert(
                (profile_id.clone(), remote_port),
                Forward {
                    local_port: entry.local_port,
                    owner: ForwardOwner::Other {
                        owner_pid: entry.owner_pid,
                    },
                },
            );
            self.next_shared_check
                .get_or_insert_with(|| Instant::now() + SHARED_RECHECK_INTERVAL);
            return;
        }
        let Some(source) = self.tunnel(profile_id) else {
            return;
        };
        let mut reserved = self
            .forwards
            .values()
            .map(|forward| forward.local_port)
            .chain(
                foreign
                    .iter()
                    // A stale entry for this remote port is taken over, not avoided.
                    .filter(|entry| {
                        &entry.profile_id != profile_id || entry.remote_port != remote_port
                    })
                    .map(|entry| entry.local_port),
            )
            .collect::<HashSet<_>>();
        // Ports remembered for other remote ports stay free for them.
        reserved.extend(
            self.remembered
                .iter()
                .filter(|entry| &entry.profile_id != profile_id || entry.remote_port != remote_port)
                .map(|entry| entry.local_port),
        );
        let remembered = self
            .remembered
            .iter()
            .find(|entry| &entry.profile_id == profile_id && entry.remote_port == remote_port)
            .map(|entry| entry.local_port);
        let candidates = remembered
            .into_iter()
            .chain((0..=NEARBY_PORTS).filter_map(|offset| remote_port.checked_add(offset)))
            .filter(|port| !reserved.contains(port))
            .chain(std::iter::once(0));
        for local_port in candidates {
            match Listener::bind(local_port, remote_port, source.clone(), &self.runtime) {
                Ok(listener) => {
                    info!(
                        remote_port,
                        local_port = listener.local_port(),
                        "opened port forward"
                    );
                    self.forwards.insert(
                        (profile_id.clone(), remote_port),
                        Forward {
                            local_port: listener.local_port(),
                            owner: ForwardOwner::Us(listener),
                        },
                    );
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(error) => {
                    warn!(remote_port, local_port, %error, "could not listen for a port forward");
                    return;
                }
            }
        }
    }

    fn live_owner_serves(&self, profile_id: &ProfileId, remote_port: u16) -> bool {
        self.registry.live_foreign_entries().iter().any(|entry| {
            &entry.profile_id == profile_id
                && entry.remote_port == remote_port
                && listener::loopback_port_accepts(entry.local_port)
        })
    }

    fn tunnel(&mut self, profile_id: &ProfileId) -> Option<Arc<TunnelSource>> {
        let profile = self.profiles.get_mut(profile_id)?;
        if profile.tunnel.is_none() {
            match (self.tunnel_factory)(&profile.endpoint) {
                Ok(source) => profile.tunnel = Some(Arc::new(source)),
                Err(error) => {
                    warn!(%error, "cannot open tunnels to saved machine");
                    return None;
                }
            }
        }
        profile.tunnel.clone()
    }

    fn persist(&self) {
        let ours = self
            .forwards
            .iter()
            .filter(|(_, forward)| matches!(forward.owner, ForwardOwner::Us(_)))
            .map(|((profile_id, remote_port), forward)| RegistryEntry {
                profile_id: profile_id.clone(),
                remote_port: *remote_port,
                local_port: forward.local_port,
                owner_pid: self.registry.own_pid(),
            })
            .collect::<Vec<_>>();
        let known = self.profiles.keys().cloned().collect::<HashSet<_>>();
        self.registry.store(&ours, &self.reported, &known);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::endpoint::EndpointAnnouncedPort;
    use std::net::Ipv4Addr;
    use std::path::PathBuf;

    struct Harness {
        runtime: tokio::runtime::Runtime,
        dir: PathBuf,
        endpoint: SavedSshEndpoint,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("herdr-port-forwards-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let mut endpoint = SavedSshEndpoint::new("workbox", "workbox", "default").unwrap();
            endpoint.enabled = true;
            Self {
                runtime: tokio::runtime::Runtime::new().unwrap(),
                dir,
                endpoint,
            }
        }

        /// A client whose own process ID is `pid`; odd IDs are running clients.
        fn client(&self, pid: u32) -> PortForwards {
            let registry =
                Registry::for_test(self.dir.join("port-forwards.json"), pid, |pid| pid % 2 == 1);
            let mut forwards =
                PortForwards::with_registry(registry, self.runtime.handle().clone(), |_| {
                    Ok(TunnelSource::Direct)
                });
            forwards.set_profiles(std::slice::from_ref(&self.endpoint));
            forwards
        }

        fn announce(&self, forwards: &mut PortForwards, ports: &[u16]) -> bool {
            forwards.receive(
                &self.endpoint.id,
                &EndpointPortAnnouncements {
                    boot_id: "boot".into(),
                    ports: ports
                        .iter()
                        .map(|port| EndpointAnnouncedPort {
                            port: *port,
                            pane_id: None,
                        })
                        .collect(),
                },
            )
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A free port with a free successor, so tests can observe the preferred port.
    fn free_port() -> u16 {
        loop {
            let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            if port < u16::MAX - NEARBY_PORTS && !listener::loopback_port_accepts(port) {
                return port;
            }
        }
    }

    fn local_ports(forwards: &PortForwards) -> Vec<(u16, u16, PortForwardState)> {
        forwards
            .views()
            .into_iter()
            .map(|view| (view.remote_port, view.local_port, view.state))
            .collect()
    }

    #[test]
    fn announced_ports_are_forwarded_to_the_same_local_port() {
        let harness = Harness::new("same");
        let mut forwards = harness.client(2);
        let port = free_port();
        assert!(harness.announce(&mut forwards, &[port]));
        assert_eq!(
            local_ports(&forwards),
            vec![(port, port, PortForwardState::Active)]
        );
        assert!(listener::loopback_port_accepts(port));
        assert!(!harness.announce(&mut forwards, &[port]), "no change");
    }

    #[test]
    fn system_ports_are_not_forwarded() {
        let harness = Harness::new("system");
        let mut forwards = harness.client(2);
        assert!(!harness.announce(&mut forwards, &[22, 80]));
        assert!(forwards.views().is_empty());
    }

    #[test]
    fn a_clash_uses_the_next_free_port() {
        let harness = Harness::new("clash");
        let mut forwards = harness.client(2);
        let port = free_port();
        let _local_program = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        harness.announce(&mut forwards, &[port]);
        let views = local_ports(&forwards);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].0, port);
        assert!(views[0].1 > port && views[0].1 <= port + NEARBY_PORTS);
    }

    #[test]
    fn withdrawn_ports_close_their_listener() {
        let harness = Harness::new("withdrawn");
        let mut forwards = harness.client(2);
        let port = free_port();
        harness.announce(&mut forwards, &[port]);
        assert!(harness.announce(&mut forwards, &[]));
        assert!(forwards.views().is_empty());
        let deadline = Instant::now() + Duration::from_secs(5);
        while listener::loopback_port_accepts(port) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!listener::loopback_port_accepts(port));
    }

    #[test]
    fn removing_the_machine_closes_its_forwards() {
        let harness = Harness::new("removed");
        let mut forwards = harness.client(2);
        harness.announce(&mut forwards, &[free_port()]);
        assert!(forwards.set_profiles(&[]));
        assert!(forwards.views().is_empty());
        assert!(!harness.announce(&mut forwards, &[free_port()]));
    }

    #[test]
    fn a_restarted_client_reuses_remembered_ports_and_forgets_closed_ones() {
        let harness = Harness::new("restart");
        let (kept, closed) = (free_port(), free_port());
        let remembered = {
            // The first run found the kept port busy, so it chose a nearby port.
            let _busy = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, kept)).unwrap();
            let mut first = harness.client(2);
            harness.announce(&mut first, &[kept, closed]);
            let views = local_ports(&first);
            let remembered = views.iter().find(|view| view.0 == kept).unwrap().1;
            assert_ne!(remembered, kept);
            remembered
        };

        // The next run prefers the remembered port although the remote port is now free.
        let mut second = harness.client(4);
        assert!(!second.remembered.is_empty());
        harness.announce(&mut second, &[kept]);
        assert_eq!(
            local_ports(&second),
            vec![(kept, remembered, PortForwardState::Active)]
        );
        let stored = second.registry.load();
        assert_eq!(stored.len(), 1, "the closed port was forgotten: {stored:?}");
        assert_eq!(stored[0].remote_port, kept);
        assert_eq!(stored[0].owner_pid, 4);
    }

    #[test]
    fn a_second_client_shows_an_existing_forward_and_takes_it_over_later() {
        let harness = Harness::new("shared");
        let port = free_port();
        let mut first = harness.client(3);
        harness.announce(&mut first, &[port]);

        let mut second = harness.client(5);
        harness.announce(&mut second, &[port]);
        assert_eq!(
            local_ports(&second),
            vec![(port, port, PortForwardState::Shared)]
        );
        assert!(second.next_shared_check.is_some());

        drop(first);
        let deadline = Instant::now() + Duration::from_secs(5);
        while listener::loopback_port_accepts(port) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(second.tick(Instant::now() + SHARED_RECHECK_INTERVAL));
        assert_eq!(
            local_ports(&second),
            vec![(port, port, PortForwardState::Active)]
        );
    }

    #[test]
    fn forwarded_connections_reach_the_remote_port() {
        use std::io::{Read as _, Write as _};

        let harness = Harness::new("carry");
        let remote = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let remote_port = remote.local_addr().unwrap().port();
        // Port probes also connect, so answer every connection.
        std::thread::spawn(move || {
            for mut stream in remote.incoming().flatten() {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            }
        });
        // The "remote" port is on this computer, so the forward must use a nearby port.
        let mut forwards = harness.client(2);
        harness.announce(&mut forwards, &[remote_port]);
        let local_port = forwards.views()[0].local_port;
        assert_ne!(local_port, remote_port);
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, local_port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert_eq!(response, b"HTTP/1.1 200 OK\r\n\r\n");
    }
}
