//! Loopback listeners that carry each accepted connection to a remote port.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tracing::{debug, warn};

const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
const EPHEMERAL_BIND_ATTEMPTS: usize = 5;
const MAX_TUNNEL_STDERR_BYTES: usize = 4096;

/// How a listener reaches the remote port.
pub(crate) enum TunnelSource {
    Ssh(crate::remote::SavedSshTunnel),
    /// Runs a command whose stdin and stdout carry the connection.
    #[cfg(test)]
    Command(fn(u16) -> std::process::Command),
    /// Connects to the port on this computer, as if it were the remote host.
    #[cfg(test)]
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TunnelHealth {
    Unknown,
    Ok,
    Failed,
}

#[derive(Default)]
struct HealthCell(AtomicU8);

impl HealthCell {
    fn set(&self, health: TunnelHealth) {
        self.0.store(health as u8, Ordering::Relaxed);
    }

    fn get(&self) -> TunnelHealth {
        match self.0.load(Ordering::Relaxed) {
            1 => TunnelHealth::Ok,
            2 => TunnelHealth::Failed,
            _ => TunnelHealth::Unknown,
        }
    }
}

/// Stops accepting and ends every open connection when dropped.
pub(crate) struct Listener {
    local_port: u16,
    health: Arc<HealthCell>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Listener {
    /// Listens on loopback `local_port`, or on a free port when `local_port` is 0.
    ///
    /// A port is in use when a local program accepts connections on it, even when the operating
    /// system would allow a more specific bind beside that program's wildcard listener.
    pub(crate) fn bind(
        local_port: u16,
        remote_port: u16,
        source: Arc<TunnelSource>,
        runtime: &tokio::runtime::Handle,
    ) -> io::Result<Self> {
        if local_port != 0 && loopback_port_accepts(local_port) {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        let attempts = if local_port == 0 {
            EPHEMERAL_BIND_ATTEMPTS
        } else {
            1
        };
        let mut last_error = io::Error::from(io::ErrorKind::AddrInUse);
        for _ in 0..attempts {
            match bind_loopback_pair(local_port) {
                Ok(listeners) => return Self::start(listeners, remote_port, source, runtime),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    fn start(
        listeners: Vec<std::net::TcpListener>,
        remote_port: u16,
        source: Arc<TunnelSource>,
        runtime: &tokio::runtime::Handle,
    ) -> io::Result<Self> {
        let local_port = listeners
            .first()
            .map(std::net::TcpListener::local_addr)
            .transpose()?
            .map_or(0, |address| address.port());
        let health = Arc::new(HealthCell::default());
        let _guard = runtime.enter();
        let mut tasks = Vec::new();
        for listener in listeners {
            listener.set_nonblocking(true)?;
            let listener = tokio::net::TcpListener::from_std(listener)?;
            let source = source.clone();
            let health = health.clone();
            tasks.push(runtime.spawn(accept_loop(listener, remote_port, source, health)));
        }
        Ok(Self {
            local_port,
            health,
            tasks,
        })
    }

    pub(crate) fn local_port(&self) -> u16 {
        self.local_port
    }

    pub(crate) fn health(&self) -> TunnelHealth {
        self.health.get()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Returns whether a program on this computer accepts connections on a loopback `port`.
pub(crate) fn loopback_port_accepts(port: u16) -> bool {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|address| TcpStream::connect_timeout(address, LOCAL_PROBE_TIMEOUT).is_ok())
}

/// Binds IPv4 loopback and, when available, IPv6 loopback on the same port, because browsers
/// may resolve `localhost` to either address.
fn bind_loopback_pair(port: u16) -> io::Result<Vec<std::net::TcpListener>> {
    let v4 = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let port = v4.local_addr()?.port();
    match std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(v6) => Ok(vec![v4, v6]),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => Err(error),
        // IPv6 loopback is optional; IPv4 alone still serves `localhost` and `127.0.0.1`.
        Err(_) => Ok(vec![v4]),
    }
}

async fn accept_loop(
    listener: tokio::net::TcpListener,
    remote_port: u16,
    source: Arc<TunnelSource>,
    health: Arc<HealthCell>,
) {
    // Dropping the set when this task is aborted ends every connection it started.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    let source = source.clone();
                    let health = health.clone();
                    connections.spawn(async move {
                        let result = carry(stream, &source, remote_port).await;
                        match result {
                            Ok(()) => health.set(TunnelHealth::Ok),
                            Err(error) => {
                                health.set(TunnelHealth::Failed);
                                warn!(remote_port, %error, "port-forward connection failed");
                            }
                        }
                    });
                }
                Err(error) => {
                    warn!(remote_port, %error, "port-forward listener stopped accepting");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn carry(
    stream: tokio::net::TcpStream,
    source: &TunnelSource,
    remote_port: u16,
) -> io::Result<()> {
    match source {
        TunnelSource::Ssh(tunnel) => {
            carry_through_process(stream, tunnel.command(remote_port)).await
        }
        #[cfg(test)]
        TunnelSource::Command(command) => carry_through_process(stream, command(remote_port)).await,
        #[cfg(test)]
        TunnelSource::Direct => {
            let remote = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, remote_port)).await?;
            let (remote_read, remote_write) = remote.into_split();
            pump(stream, remote_read, remote_write).await;
            Ok(())
        }
    }
}

async fn carry_through_process(
    stream: tokio::net::TcpStream,
    command: std::process::Command,
) -> io::Result<()> {
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let (Some(stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(io::Error::other("tunnel process pipes are missing"));
    };
    let stderr_reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = (&mut stderr)
            .take(MAX_TUNNEL_STDERR_BYTES as u64)
            .read_to_end(&mut bytes)
            .await;
        bytes
    });
    pump(stream, stdout, stdin).await;
    let status = child.wait().await?;
    let stderr = stderr_reader.await.unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&stderr);
    let message = message.trim();
    debug!(%status, stderr = %message, "tunnel process exited");
    Err(io::Error::other(if message.is_empty() {
        format!("tunnel exited with {status}")
    } else {
        message.to_owned()
    }))
}

/// Copies both directions until each side reaches end of file, preserving half-close.
async fn pump(
    local: tokio::net::TcpStream,
    mut remote_read: impl AsyncRead + Unpin,
    mut remote_write: impl AsyncWrite + Unpin,
) {
    let (mut local_read, mut local_write) = local.into_split();
    let upstream = async move {
        let _ = tokio::io::copy(&mut local_read, &mut remote_write).await;
        let _ = remote_write.shutdown().await;
        // Dropping the writer closes a pipe, which is how a process sees end of input.
        drop(remote_write);
    };
    let downstream = async move {
        let _ = tokio::io::copy(&mut remote_read, &mut local_write).await;
        let _ = local_write.shutdown().await;
    };
    tokio::join!(upstream, downstream);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    fn echo_server() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn round_trip(port: u16, payload: &[u8]) -> Vec<u8> {
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(payload).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).unwrap();
        received
    }

    #[test]
    fn direct_tunnels_carry_bytes_both_ways_with_half_close() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (remote, remote_port) = echo_server();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = remote.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            stream.write_all(b"reply:").unwrap();
            stream.write_all(&request).unwrap();
        });
        let listener = Listener::bind(
            0,
            remote_port,
            Arc::new(TunnelSource::Direct),
            runtime.handle(),
        )
        .unwrap();
        assert_eq!(round_trip(listener.local_port(), b"hello"), b"reply:hello");
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn process_tunnels_use_stdio_and_report_health() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = Listener::bind(
            0,
            1,
            Arc::new(TunnelSource::Command(|_| std::process::Command::new("cat"))),
            runtime.handle(),
        )
        .unwrap();
        assert_eq!(listener.health(), TunnelHealth::Unknown);
        assert_eq!(round_trip(listener.local_port(), b"echo"), b"echo");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while listener.health() == TunnelHealth::Unknown && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(listener.health(), TunnelHealth::Ok);

        let failing = Listener::bind(
            0,
            1,
            Arc::new(TunnelSource::Command(|_| {
                let mut command = std::process::Command::new("sh");
                command.arg("-c").arg("echo 'open failed' >&2; exit 255");
                command
            })),
            runtime.handle(),
        )
        .unwrap();
        assert_eq!(round_trip(failing.local_port(), b"x"), b"");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while failing.health() == TunnelHealth::Unknown && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(failing.health(), TunnelHealth::Failed);
    }

    #[test]
    fn a_port_with_a_local_program_is_in_use() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (_occupied, port) = echo_server();
        let error = Listener::bind(port, 1, Arc::new(TunnelSource::Direct), runtime.handle())
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn dropping_a_listener_frees_its_port() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener =
            Listener::bind(0, 1, Arc::new(TunnelSource::Direct), runtime.handle()).unwrap();
        let port = listener.local_port();
        assert!(loopback_port_accepts(port));
        drop(listener);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while loopback_port_accepts(port) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!loopback_port_accepts(port));
    }
}
