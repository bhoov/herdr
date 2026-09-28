//! Detect loopback addresses that pane output announces, such as `http://localhost:5173/`.
//!
//! The tracker sees raw PTY bytes. It removes escape sequences, keeps the visible text of the
//! current line, and reports each loopback port once per line. SGR sequences do not separate
//! text because tools often style only the port (`localhost:\e[1m5173\e[22m`). Other control
//! sequences, such as cursor motion, separate words so unrelated screen fragments do not join.

use std::time::{Duration, Instant};

const MAX_LINE_BYTES: usize = 512;
/// Bytes kept after an overlong line is scanned, so an address split at the cut still matches.
const LINE_OVERLAP_BYTES: usize = 32;
/// Full-screen programs redraw the same text often; report a port again only after this delay.
const REPEAT_SUPPRESSION: Duration = Duration::from_secs(5);
const MAX_RECENT_PORTS: usize = 32;
const LOOPBACK_HOSTS: [&[u8]; 5] = [b"localhost", b"127.0.0.1", b"0.0.0.0", b"[::1]", b"[::]"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Csi,
    /// OSC, DCS, APC, PM, or SOS payload: invisible until BEL or ST.
    String,
    StringEscape,
}

#[derive(Debug, Default)]
pub(crate) struct PortAnnouncementTracker {
    state: ScanState,
    line: Vec<u8>,
    line_ports: Vec<u16>,
    pending: Vec<u16>,
    recent: Vec<(u16, Instant)>,
}

impl PortAnnouncementTracker {
    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                ScanState::Ground => match byte {
                    0x1b => self.state = ScanState::Escape,
                    b'\n' | b'\r' => self.finish_line(),
                    0x00..=0x1f | 0x7f => self.push_separator(),
                    _ => self.push(byte),
                },
                ScanState::Escape => {
                    self.state = match byte {
                        b'[' => ScanState::Csi,
                        b']' | b'P' | b'_' | b'^' | b'X' => ScanState::String,
                        // Charset selection and similar sequences take one more byte.
                        b'(' | b')' | b'*' | b'+' | b'#' | b'%' => ScanState::Escape,
                        _ => {
                            self.push_separator();
                            ScanState::Ground
                        }
                    };
                }
                ScanState::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        if byte != b'm' {
                            self.push_separator();
                        }
                        self.state = ScanState::Ground;
                    }
                }
                ScanState::String => match byte {
                    0x07 => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::StringEscape,
                    _ => {}
                },
                ScanState::StringEscape => {
                    self.state = if byte == b'\\' {
                        ScanState::Ground
                    } else {
                        ScanState::String
                    };
                }
            }
        }
        self.scan(false);
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Returns ports first announced since the last call, without recent repeats.
    pub(crate) fn take_announced(&mut self, now: Instant) -> Vec<u16> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.recent
            .retain(|(_, at)| now.saturating_duration_since(*at) < REPEAT_SUPPRESSION);
        let mut announced = Vec::new();
        for port in std::mem::take(&mut self.pending) {
            if self.recent.iter().any(|(recent, _)| *recent == port) || announced.contains(&port) {
                continue;
            }
            if self.recent.len() >= MAX_RECENT_PORTS {
                self.recent.remove(0);
            }
            self.recent.push((port, now));
            announced.push(port);
        }
        announced
    }

    fn push(&mut self, byte: u8) {
        if self.line.len() >= MAX_LINE_BYTES {
            self.scan(false);
            let keep_from = self.line.len().saturating_sub(LINE_OVERLAP_BYTES);
            self.line.drain(..keep_from);
        }
        self.line.push(byte);
    }

    fn push_separator(&mut self) {
        if self.line.last().is_some_and(|last| *last != b' ') {
            self.push(b' ');
        }
    }

    fn finish_line(&mut self) {
        self.scan(true);
        self.line.clear();
        self.line_ports.clear();
    }

    /// With `complete`, the line end terminates a trailing port; otherwise more digits may follow.
    fn scan(&mut self, complete: bool) {
        for colon in byte_positions(b':', &self.line) {
            let Some(port) = loopback_port_at(&self.line, colon, complete) else {
                continue;
            };
            if !self.line_ports.contains(&port) {
                self.line_ports.push(port);
                self.pending.push(port);
            }
        }
    }
}

fn byte_positions(needle: u8, haystack: &[u8]) -> impl Iterator<Item = usize> + '_ {
    haystack
        .iter()
        .enumerate()
        .filter(move |(_, byte)| **byte == needle)
        .map(|(index, _)| index)
}

/// Returns the port after the colon at `colon` when a loopback host precedes it.
fn loopback_port_at(line: &[u8], colon: usize, complete: bool) -> Option<u16> {
    let before = &line[..colon];
    let host = LOOPBACK_HOSTS.iter().find(|host| {
        before.len() >= host.len() && before[before.len() - host.len()..].eq_ignore_ascii_case(host)
    })?;
    let host_start = colon - host.len();
    let bounded = host[0] == b'[' || host_start == 0 || !is_host_byte(line[host_start - 1]);
    if !bounded {
        return None;
    }
    let digits_start = colon + 1;
    let digits = line[digits_start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let digits_end = digits_start + digits;
    let terminated = match line.get(digits_end) {
        Some(next) => !next.is_ascii_alphanumeric(),
        None => complete,
    };
    if digits == 0 || digits > 5 || !terminated {
        return None;
    }
    std::str::from_utf8(&line[digits_start..digits_end])
        .ok()?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
}

fn is_host_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announced(chunks: &[&[u8]]) -> Vec<u16> {
        let mut tracker = PortAnnouncementTracker::default();
        let mut ports = Vec::new();
        let now = Instant::now();
        for chunk in chunks {
            tracker.observe(chunk);
            ports.extend(tracker.take_announced(now));
        }
        ports
    }

    #[test]
    fn detects_common_dev_server_banners() {
        assert_eq!(
            announced(&[
                b"  VITE v5  ready\r\n  \xe2\x9e\x9c  Local:   http://localhost:5173/\r\n"
            ]),
            vec![5173]
        );
        assert_eq!(
            announced(&[b"Listening at http://127.0.0.1:8000 (Press CTRL+C to quit)\n"]),
            vec![8000]
        );
        assert_eq!(announced(&[b"Serving on 0.0.0.0:3000\n"]), vec![3000]);
        assert_eq!(
            announced(&[b"bound to [::1]:4000, [::]:4001\n"]),
            vec![4000, 4001]
        );
        assert_eq!(announced(&[b"http://LOCALHOST:9000\n"]), vec![9000]);
    }

    #[test]
    fn styled_ports_match_across_sgr_sequences() {
        assert_eq!(
            announced(&[b"http://localhost:\x1b[1m5173\x1b[22m/\r\n"]),
            vec![5173]
        );
    }

    #[test]
    fn cursor_motion_separates_fragments() {
        assert_eq!(announced(&[b"localhost:\x1b[5C3000\n"]), Vec::<u16>::new());
    }

    #[test]
    fn osc_payloads_are_invisible() {
        assert_eq!(
            announced(&[b"\x1b]8;;http://localhost:1111/\x1b\\link\x1b]8;;\x07 done\n"]),
            Vec::<u16>::new()
        );
        assert_eq!(
            announced(&[b"\x1b]0;localhost:2222\x07http://localhost:3333\n"]),
            vec![3333]
        );
    }

    #[test]
    fn ports_split_across_reads_wait_for_a_terminator() {
        assert_eq!(announced(&[b"http://localhost:51", b"73/\n"]), vec![5173]);
        assert_eq!(
            announced(&[b"http://local", b"host:8080", b"\n"]),
            vec![8080]
        );
    }

    #[test]
    fn a_trailing_port_is_reported_at_line_end() {
        let mut tracker = PortAnnouncementTracker::default();
        tracker.observe(b"listening on localhost:8080");
        assert!(tracker.take_announced(Instant::now()).is_empty());
        tracker.observe(b"\r\n");
        assert_eq!(tracker.take_announced(Instant::now()), vec![8080]);
    }

    #[test]
    fn rejects_non_loopback_and_malformed_ports() {
        for line in [
            &b"http://192.168.1.5:5173/\n"[..],
            b"http://mylocalhost:5173/\n",
            b"http://10.0.0.0:5173/\n",
            b"localhost:0\n",
            b"localhost:70000\n",
            b"localhost:123456\n",
            b"localhost:3000abc\n",
            b"localhost: 3000\n",
            b"time 12:30:45\n",
        ] {
            assert_eq!(announced(&[line]), Vec::<u16>::new(), "{line:?}");
        }
    }

    #[test]
    fn repeated_redraws_are_suppressed_until_the_delay_expires() {
        let mut tracker = PortAnnouncementTracker::default();
        let start = Instant::now();
        tracker.observe(b"localhost:3000\r\n");
        assert_eq!(tracker.take_announced(start), vec![3000]);
        tracker.observe(b"localhost:3000\r\nlocalhost:3000\r\n");
        assert!(tracker
            .take_announced(start + Duration::from_secs(1))
            .is_empty());
        tracker.observe(b"localhost:3000\r\n");
        assert_eq!(
            tracker.take_announced(start + REPEAT_SUPPRESSION),
            vec![3000]
        );
    }

    /// Throughput check for the PTY hot path: `cargo test --release --bin herdr
    /// port_scan_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn port_scan_throughput() {
        let mut sample = Vec::new();
        for index in 0..2000 {
            sample.extend_from_slice(
                format!(
                    "\x1b[32m{index:05}\x1b[0m {{\"ts\":\"12:30:45\",\"url\":\"http://example.com:80/a\"}} plain log text\r\n"
                )
                .as_bytes(),
            );
            sample.extend_from_slice(
                b"\x1b[2;5H\x1b[1mredraw\x1b[22m key:value key:value key:value ",
            );
        }
        let mut tracker = PortAnnouncementTracker::default();
        let rounds = 200;
        let started = Instant::now();
        for _ in 0..rounds {
            for chunk in sample.chunks(4096) {
                tracker.observe(chunk);
            }
        }
        let elapsed = started.elapsed();
        let megabytes = (sample.len() * rounds) as f64 / 1e6;
        println!(
            "port scan: {megabytes:.0} MB in {elapsed:?} = {:.0} MB/s",
            megabytes / elapsed.as_secs_f64()
        );
    }

    #[test]
    fn overlong_lines_keep_matching_without_growing() {
        let mut tracker = PortAnnouncementTracker::default();
        tracker.observe(&[b'x'; MAX_LINE_BYTES * 4]);
        tracker.observe(b" http://localhost:7000/ ");
        tracker.observe(&[b'y'; MAX_LINE_BYTES * 2]);
        assert!(tracker.line.len() <= MAX_LINE_BYTES);
        assert_eq!(tracker.take_announced(Instant::now()), vec![7000]);
    }
}
