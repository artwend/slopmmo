//! Client-side UDP link.
//!
//! Non-blocking on both ends: sending never waits for the server, and receiving
//! is drained once per frame. Losing a report is fine — the next one re-states
//! the whole pose.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

use proto::{PlayerMovementPacket, ServerSnapshot};

pub struct ClientLink {
    socket: UdpSocket,
    server: SocketAddr,
    pub player_id: u64,
    recv_buf: Vec<u8>,
    pub sent: u64,
    pub received: u64,
    pub malformed: u64,
}

impl ClientLink {
    pub fn connect(addr: &str, player_id: u64) -> std::io::Result<Self> {
        let server = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("no address resolved"))?;
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        // Connecting a UDP socket filters inbound datagrams to this peer, so a
        // stray packet from anywhere else can never reach the decode path.
        socket.connect(server)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            server,
            player_id,
            recv_buf: vec![0u8; 1500],
            sent: 0,
            received: 0,
            malformed: 0,
        })
    }

    /// Address this socket is connected to; inbound traffic from anyone else is dropped by the kernel.
    pub fn peer(&self) -> SocketAddr {
        self.server
    }

    pub fn send(&mut self, packet: &PlayerMovementPacket) {
        if self.socket.send(&packet.encode()).is_ok() {
            self.sent += 1;
        }
    }

    pub fn drain_snapshots(&mut self) -> Vec<ServerSnapshot> {
        let mut out = Vec::new();
        loop {
            match self.socket.recv(&mut self.recv_buf) {
                Ok(n) => match ServerSnapshot::decode(&self.recv_buf[..n]) {
                    Ok(s) => {
                        self.received += 1;
                        out.push(s);
                    }
                    Err(_) => self.malformed += 1,
                },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_drain_decodes_what_the_server_would_send() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(false).unwrap();
        let addr = server.local_addr().unwrap();
        let mut link = ClientLink::connect(&addr.to_string(), 1).unwrap();

        // The client announces itself first; that is how the server learns the
        // ephemeral port it has to reply to.
        link.send(&PlayerMovementPacket {
            player: 1,
            seq: 1,
            ..Default::default()
        });
        let mut buf = [0u8; 1500];
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let (_, client_addr) = server.recv_from(&mut buf).expect("no announce packet");

        let snap = ServerSnapshot {
            server_tick: proto::Tick::new(11),
            ..Default::default()
        };
        server.send_to(&snap.encode(), client_addr).unwrap();

        for _ in 0..50 {
            let got = link.drain_snapshots();
            if !got.is_empty() {
                assert_eq!(got[0].server_tick, proto::Tick::new(11));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("snapshot never arrived");
    }
}
