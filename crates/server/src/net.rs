//! UDP transport, one socket, one thread.
//!
//! Deliberately dumb: no channels, no serialisation layer, no reliability on
//! top of UDP. The protocol in `proto` already carries its own redundancy and
//! sequence numbers, so this file only moves bytes.

use std::net::{SocketAddr, UdpSocket};

use proto::{PlayerMovementPacket, ServerSnapshot};

/// Windows reports `WSAECONNRESET` on every poll of a socket whose peer port
/// has gone away, so a crashed client would otherwise log a warning per frame.
/// Report the first error of a kind, stay quiet until traffic flows again.
#[derive(Default)]
struct ErrorSpam(Option<std::io::ErrorKind>);

impl ErrorSpam {
    fn should_report(&mut self, kind: std::io::ErrorKind) -> bool {
        if self.0 == Some(kind) {
            return false;
        }
        self.0 = Some(kind);
        true
    }

    fn clear(&mut self) {
        self.0 = None;
    }
}

pub struct UdpServer {
    socket: UdpSocket,
    recv_buf: Vec<u8>,
    last_seq: std::collections::HashMap<u64, u16>,
    recv_spam: ErrorSpam,
    send_spam: ErrorSpam,
    pub packets_in: u64,
    pub packets_out: u64,
    pub bytes_in: u64,
}

impl UdpServer {
    pub fn bind(addr: &str) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            recv_buf: vec![0u8; 1500],
            last_seq: Default::default(),
            recv_spam: ErrorSpam::default(),
            send_spam: ErrorSpam::default(),
            packets_in: 0,
            packets_out: 0,
            bytes_in: 0,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Drains the socket. Returns every decoded packet with its sender.
    pub fn drain(&mut self) -> Vec<(SocketAddr, PlayerMovementPacket)> {
        let mut out = Vec::new();
        loop {
            match self.socket.recv_from(&mut self.recv_buf) {
                Ok((n, from)) => {
                    self.packets_in += 1;
                    self.bytes_in += n as u64;
                    self.recv_spam.clear();
                    match PlayerMovementPacket::decode(&self.recv_buf[..n]) {
                        Ok(pkt) => {
                            // Transport-level de-duplication: the sim also
                            // rejects duplicates, but doing it here saves the
                            // validation work entirely.
                            let e = self.last_seq.entry(pkt.player).or_insert(0);
                            if pkt.seq <= *e && e.wrapping_sub(pkt.seq) < 0x8000 {
                                continue;
                            }
                            *e = pkt.seq;
                            out.push((from, pkt));
                        }
                        Err(err) => {
                            bevy::log::debug!("dropping malformed packet from {from}: {err:?}");
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    if self.recv_spam.should_report(e.kind()) {
                        bevy::log::warn!("recv failed: {e} (suppressing repeats)");
                    }
                    break;
                }
            }
        }
        out
    }

    pub fn send(&mut self, to: SocketAddr, snapshot: &ServerSnapshot) {
        let bytes = snapshot.encode();
        if let Err(e) = self.socket.send_to(&bytes, to) {
            if self.send_spam.should_report(e.kind()) {
                bevy::log::warn!("send to {to} failed: {e} (suppressing repeats)");
            }
        } else {
            self.send_spam.clear();
            self.packets_out += 1;
        }
    }

    pub fn forget(&mut self, player: u64) {
        self.last_seq.remove(&player);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_roundtrip() {
        let mut server = UdpServer::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.connect(addr).unwrap();

        let pkt = PlayerMovementPacket {
            seq: 1,
            player: 7,
            report: proto::MovementReport {
                tick: proto::Tick::new(5),
                position: [1.0, 0.9, 2.0],
                ..Default::default()
            },
            inputs: Vec::new(),
        };
        client.send(&pkt.encode()).unwrap();

        // Loopback is fast but not instant.
        for _ in 0..50 {
            let got = server.drain();
            if !got.is_empty() {
                assert_eq!(got[0].1.player, 7);
                assert_eq!(got[0].1.seq, 1);
                server.send(got[0].0, &ServerSnapshot::default());
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("packet never arrived");
    }

    #[test]
    fn repeated_transport_errors_are_reported_once() {
        let mut spam = ErrorSpam::default();
        let kind = std::io::ErrorKind::ConnectionReset;
        assert!(spam.should_report(kind), "first error must be visible");
        assert!(!spam.should_report(kind), "repeat must stay quiet");
        assert!(spam.should_report(std::io::ErrorKind::ConnectionAborted));

        spam.should_report(kind);
        spam.clear();
        assert!(
            spam.should_report(kind),
            "traffic again re-arms the warning"
        );
    }

    #[test]
    fn duplicate_sequence_numbers_are_dropped_at_the_transport() {
        let mut server = UdpServer::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.connect(addr).unwrap();

        for _ in 0..2 {
            let pkt = PlayerMovementPacket {
                seq: 3,
                player: 9,
                ..Default::default()
            };
            client.send(&pkt.encode()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(server.drain().len(), 1, "duplicate was not dropped");
    }
}
