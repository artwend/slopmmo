//! A transparent UDP relay that reports every datagram it forwards.
//!
//! The relay is how the analyzer sees loopback traffic without Npcap or
//! administrator rights: the client connects to the relay instead of the
//! server, and the relay keeps one upstream socket per client address so the
//! server still sees a normal UDP peer.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use crate::message::Direction;

const MAX_DATAGRAM: usize = 65_535;
/// Bounds one poll pass so a flood on the downstream side cannot starve the
/// client side or the stats timer.
const DRAIN_BUDGET_PER_LINK: usize = 64;

pub struct Relay {
    front: UdpSocket,
    server: SocketAddr,
    clients: HashMap<SocketAddr, UdpSocket>,
    up_buf: Vec<u8>,
    down_buf: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct Datagram<'a> {
    pub direction: Direction,
    pub from: SocketAddr,
    pub to: SocketAddr,
    pub bytes: &'a [u8],
}

impl Relay {
    pub fn bind(listen: SocketAddr, server: SocketAddr) -> io::Result<Self> {
        let front = UdpSocket::bind(listen)?;
        front.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self {
            front,
            server,
            clients: HashMap::new(),
            up_buf: vec![0; MAX_DATAGRAM],
            down_buf: vec![0; MAX_DATAGRAM],
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.front.local_addr()
    }

    /// Waits up to `timeout` for one client datagram, forwards it, then drains
    /// every upstream socket. The callback sees the bytes unchanged, in the
    /// direction they were travelling.
    pub fn poll(
        &mut self,
        timeout: Duration,
        on_datagram: &mut dyn FnMut(Datagram<'_>),
    ) -> io::Result<()> {
        self.front
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;

        match self.front.recv_from(&mut self.up_buf) {
            Ok((len, client)) => {
                self.forward_up(client, len)?;
                on_datagram(Datagram {
                    direction: Direction::ClientToServer,
                    from: client,
                    to: self.server,
                    bytes: &self.up_buf[..len],
                });
            }
            Err(e) if is_disconnect_noise(&e) => {}
            Err(e) => return Err(e),
        }

        self.drain_down(on_datagram)
    }

    fn forward_up(&mut self, client: SocketAddr, len: usize) -> io::Result<()> {
        if !self.clients.contains_key(&client) {
            let socket = UdpSocket::bind(any_addr(self.server))?;
            socket.set_nonblocking(true)?;
            self.clients.insert(client, socket);
        }
        let link = self.clients.get(&client).expect("link was just inserted");
        link.send_to(&self.up_buf[..len], self.server)?;
        Ok(())
    }

    fn drain_down(&mut self, on_datagram: &mut dyn FnMut(Datagram<'_>)) -> io::Result<()> {
        let front = &self.front;
        let clients = &self.clients;
        let buf = &mut self.down_buf;

        for (client, link) in clients {
            for _ in 0..DRAIN_BUDGET_PER_LINK {
                match link.recv_from(buf) {
                    Ok((len, from)) => {
                        front.send_to(&buf[..len], *client)?;
                        on_datagram(Datagram {
                            direction: Direction::ServerToClient,
                            from,
                            to: *client,
                            bytes: &buf[..len],
                        });
                    }
                    Err(e) if is_disconnect_noise(&e) => break,
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }
}

fn any_addr(peer: SocketAddr) -> SocketAddr {
    if peer.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    }
}

fn is_disconnect_noise(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_forwards_both_ways_and_reports_each_direction() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server_addr = server.local_addr().unwrap();

        let mut relay = Relay::bind("127.0.0.1:0".parse().unwrap(), server_addr).unwrap();
        let relay_addr = relay.local_addr().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let client_addr = client.local_addr().unwrap();

        type Seen = (Direction, Vec<u8>, SocketAddr, SocketAddr);
        fn record(seen: &mut Vec<Seen>, d: Datagram<'_>) {
            seen.push((d.direction, d.bytes.to_vec(), d.from, d.to));
        }
        let mut seen: Vec<Seen> = Vec::new();

        client.send_to(&[1, 2, 3], relay_addr).unwrap();
        relay
            .poll(Duration::from_millis(50), &mut |d| record(&mut seen, d))
            .unwrap();

        let mut buf = [0u8; 16];
        let (len, from) = server.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..len], &[1, 2, 3]);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, Direction::ClientToServer);
        assert_eq!(seen[0].1, [1, 2, 3]);
        assert_eq!(seen[0].2, client_addr);
        assert_eq!(seen[0].3, server_addr);

        server.send_to(&[9, 8], from).unwrap();
        relay
            .poll(Duration::from_millis(50), &mut |d| record(&mut seen, d))
            .unwrap();

        let (len, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..len], &[9, 8]);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].0, Direction::ServerToClient);
        assert_eq!(seen[1].1, [9, 8]);
        assert_eq!(seen[1].2, server_addr);
        assert_eq!(seen[1].3, client_addr);
    }

    #[test]
    fn relay_keeps_two_clients_on_separate_upstream_sockets() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server_addr = server.local_addr().unwrap();

        let mut relay = Relay::bind("127.0.0.1:0".parse().unwrap(), server_addr).unwrap();
        let relay_addr = relay.local_addr().unwrap();

        let client_a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_b = UdpSocket::bind("127.0.0.1:0").unwrap();
        client_a
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client_b
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let addr_a = client_a.local_addr().unwrap();
        let addr_b = client_b.local_addr().unwrap();

        type Seen = (Direction, Vec<u8>, SocketAddr, SocketAddr);
        fn record(seen: &mut Vec<Seen>, d: Datagram<'_>) {
            seen.push((d.direction, d.bytes.to_vec(), d.from, d.to));
        }
        let mut seen: Vec<Seen> = Vec::new();

        client_a.send_to(&[1], relay_addr).unwrap();
        relay
            .poll(Duration::from_millis(50), &mut |d| record(&mut seen, d))
            .unwrap();
        client_b.send_to(&[2], relay_addr).unwrap();
        relay
            .poll(Duration::from_millis(50), &mut |d| record(&mut seen, d))
            .unwrap();

        let mut buf = [0u8; 16];
        let mut upstream = HashMap::new();
        for _ in 0..2 {
            let (len, from) = server.recv_from(&mut buf).unwrap();
            assert_eq!(len, 1);
            upstream.insert(buf[0], from);
        }
        let from_a = upstream[&1];
        let from_b = upstream[&2];
        assert_ne!(from_a, from_b, "two clients shared one upstream socket");

        server.send_to(&[10], from_a).unwrap();
        server.send_to(&[20], from_b).unwrap();
        relay
            .poll(Duration::from_millis(50), &mut |d| record(&mut seen, d))
            .unwrap();

        let (len, _) = client_a.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..len], &[10]);
        let (len, _) = client_b.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..len], &[20]);

        assert_eq!(seen.len(), 4);
        for (direction, bytes, from, to) in &seen {
            match (direction, bytes.as_slice()) {
                (Direction::ClientToServer, [1]) => assert_eq!(*from, addr_a),
                (Direction::ClientToServer, [2]) => assert_eq!(*from, addr_b),
                (Direction::ServerToClient, [10]) => {
                    assert_eq!(*from, server_addr);
                    assert_eq!(*to, addr_a);
                }
                (Direction::ServerToClient, [20]) => {
                    assert_eq!(*from, server_addr);
                    assert_eq!(*to, addr_b);
                }
                other => panic!("unexpected relay event: {other:?}"),
            }
        }
    }
}
