//! The port-forward registry file shared by the clients on this computer.
//!
//! Each entry maps a saved machine's remote port to a local port and names the client process
//! that owns the local listener. Entries of exited clients remain as hints, so a restarted
//! client reuses the same local ports. Entries of running clients stop a second client from
//! forwarding the same remote port again.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::client::endpoint::ProfileId;

const REGISTRY_VERSION: u32 = 1;
const MAX_REGISTRY_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegistryEntry {
    pub(crate) profile_id: ProfileId,
    pub(crate) remote_port: u16,
    pub(crate) local_port: u16,
    pub(crate) owner_pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    #[serde(default)]
    forwards: Vec<RegistryEntry>,
}

pub(crate) struct Registry {
    path: PathBuf,
    own_pid: u32,
    process_alive: fn(u32) -> bool,
}

impl Registry {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            own_pid: std::process::id(),
            process_alive: crate::platform::process_exists,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf, own_pid: u32, process_alive: fn(u32) -> bool) -> Self {
        Self {
            path,
            own_pid,
            process_alive,
        }
    }

    pub(crate) fn default_path() -> PathBuf {
        crate::config::state_dir()
            .join("client")
            .join("port-forwards.json")
    }

    pub(crate) fn own_pid(&self) -> u32 {
        self.own_pid
    }

    pub(crate) fn owner_alive(&self, pid: u32) -> bool {
        pid != self.own_pid && (self.process_alive)(pid)
    }

    /// Reads the registry. A missing or unreadable file is an empty registry: forwarding still
    /// works, only without remembered ports.
    pub(crate) fn load(&self) -> Vec<RegistryEntry> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                warn!(path = %self.path.display(), %error, "port-forward registry is unreadable");
                return Vec::new();
            }
        };
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            warn!(path = %self.path.display(), "port-forward registry exceeds the size limit");
            return Vec::new();
        }
        match serde_json::from_slice::<RegistryFile>(&bytes) {
            Ok(file) if file.version == REGISTRY_VERSION => file.forwards,
            Ok(file) => {
                warn!(
                    version = file.version,
                    "unsupported port-forward registry version"
                );
                Vec::new()
            }
            Err(error) => {
                warn!(path = %self.path.display(), %error, "port-forward registry is invalid");
                Vec::new()
            }
        }
    }

    /// Entries that belong to other running clients.
    pub(crate) fn live_foreign_entries(&self) -> Vec<RegistryEntry> {
        self.load()
            .into_iter()
            .filter(|entry| self.owner_alive(entry.owner_pid))
            .collect()
    }

    /// Replaces this client's entries with `ours`.
    ///
    /// Other clients' entries remain while their owner runs. Hints from exited clients remain
    /// only for machines that have not reported their ports in this session, because a report
    /// makes this client's state authoritative for that machine. Entries of unknown machines
    /// are removed.
    pub(crate) fn store(
        &self,
        ours: &[RegistryEntry],
        reported: &HashSet<ProfileId>,
        known_profiles: &HashSet<ProfileId>,
    ) {
        let mut forwards = self
            .load()
            .into_iter()
            .filter(|entry| {
                entry.owner_pid != self.own_pid
                    && known_profiles.contains(&entry.profile_id)
                    && !ours.iter().any(|own| {
                        own.profile_id == entry.profile_id && own.remote_port == entry.remote_port
                    })
                    && ((self.process_alive)(entry.owner_pid)
                        || !reported.contains(&entry.profile_id))
            })
            .collect::<Vec<_>>();
        forwards.extend_from_slice(ours);
        let file = RegistryFile {
            version: REGISTRY_VERSION,
            forwards,
        };
        let result = serde_json::to_vec_pretty(&file)
            .map_err(|error| error.to_string())
            .and_then(|content| {
                crate::client::endpoint::store_private_json(
                    &self.path,
                    &content,
                    "port-forward registry",
                )
            });
        if let Err(error) = result {
            warn!(path = %self.path.display(), %error, "failed to store port-forward registry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(byte: char) -> ProfileId {
        ProfileId::parse(byte.to_string().repeat(32)).unwrap()
    }

    fn entry(profile_id: &ProfileId, remote_port: u16, owner_pid: u32) -> RegistryEntry {
        RegistryEntry {
            profile_id: profile_id.clone(),
            remote_port,
            local_port: remote_port,
            owner_pid,
        }
    }

    fn registry(name: &str, own_pid: u32) -> Registry {
        let dir =
            std::env::temp_dir().join(format!("herdr-port-registry-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Odd process IDs are running clients in these tests.
        Registry::for_test(dir.join("port-forwards.json"), own_pid, |pid| pid % 2 == 1)
    }

    #[test]
    fn missing_and_invalid_files_are_empty() {
        let registry = registry("invalid", 2);
        assert!(registry.load().is_empty());
        std::fs::create_dir_all(registry.path.parent().unwrap()).unwrap();
        std::fs::write(&registry.path, b"not json").unwrap();
        assert!(registry.load().is_empty());
        std::fs::write(&registry.path, br#"{"version":99,"forwards":[]}"#).unwrap();
        assert!(registry.load().is_empty());
    }

    #[test]
    fn store_keeps_running_clients_and_unreported_hints() {
        let (a, b, gone) = (profile('a'), profile('b'), profile('c'));
        let first = registry("merge", 2);
        let known = HashSet::from([a.clone(), b.clone()]);
        first.store(
            &[
                entry(&a, 3000, 1),
                entry(&a, 4000, 4),
                entry(&b, 5000, 4),
                entry(&gone, 6000, 1),
            ],
            &HashSet::new(),
            &HashSet::from([a.clone(), b.clone(), gone.clone()]),
        );

        // Profile `a` reported, so its exited hint (pid 4) is dropped; `b` did not report.
        first.store(&[entry(&a, 7000, 2)], &HashSet::from([a.clone()]), &known);
        let mut stored = first.load();
        stored.sort_by_key(|entry| entry.remote_port);
        assert_eq!(
            stored,
            vec![entry(&a, 3000, 1), entry(&b, 5000, 4), entry(&a, 7000, 2)]
        );
        assert_eq!(first.live_foreign_entries(), vec![entry(&a, 3000, 1)]);
    }

    #[test]
    fn our_entry_replaces_any_other_entry_for_the_same_remote_port() {
        let a = profile('a');
        let registry = registry("replace", 2);
        let known = HashSet::from([a.clone()]);
        registry.store(&[entry(&a, 3000, 4)], &HashSet::new(), &known);
        let mut ours = entry(&a, 3000, 2);
        ours.local_port = 3001;
        registry.store(std::slice::from_ref(&ours), &HashSet::new(), &known);
        assert_eq!(registry.load(), vec![ours]);
    }
}
