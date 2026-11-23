//! Lightyear transport for the client.
//!
//! The client is a raw connection: lightyear provides the UDP link, packet
//! assembly and the message plumbing, and everything here is game policy.

use std::net::SocketAddr;
use std::time::Duration;

use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::*;

use crate::Local;

/// Systems that touch the wire, exposed so the tick can run between them:
/// snapshots are drained before the next solve, reports are written after it.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetSet {
    Receive,
    Send,
}

#[derive(Resource, Default)]
pub struct NetStats {
    pub sent: u64,
    pub received: u64,
    pub corrections: u64,
    pub resyncs: u64,
    /// Newest report sequence the server acknowledged.
    pub acked: u16,
    /// Reports the server had not acknowledged when the last snapshot arrived;
    /// an estimate of what is in flight (or lost) in the uplink.
    pub unacked: u16,
    /// Links torn down locally.
    pub disconnects: u64,
}

pub struct ClientNetPlugin {
    pub server: SocketAddr,
    pub player_id: u64,
}

#[derive(Resource)]
struct Server(SocketAddr);

#[derive(Resource)]
struct PlayerId(u64);

impl Plugin for ClientNetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ClientPlugins {
            tick_duration: net::CLIENT_TICK_DURATION,
        })
        .add_plugins(net::MovementProtocol)
        .configure_sets(Update, (NetSet::Receive, NetSet::Send).chain())
        .init_resource::<NetStats>()
        .insert_resource(Server(self.server))
        .insert_resource(PlayerId(self.player_id))
        .add_systems(Startup, spawn_client)
        .add_systems(
            Update,
            (
                drain_snapshots.in_set(NetSet::Receive),
                report_disconnects,
                uplink.in_set(NetSet::Send),
            ),
        );
    }
}

fn spawn_client(mut commands: Commands, server: Res<Server>) {
    // Port 0: let the OS pick, the server learns the address from the packet. The bind address
    // must stay unspecified, not loopback, or the socket cannot reach a non-local server.
    let client = commands
        .spawn((
            RawClient,
            UdpIo::default(),
            LocalAddr(SocketAddr::from(([0, 0, 0, 0], 0))),
            PeerAddr(server.0),
        ))
        .id();
    info!("connecting to {}", server.0);
    commands.trigger(Connect { entity: client });
}

/// One-way delay in client ticks, from the link's measured round trip.
///
/// A snapshot describes the tick it was built on, so by the time it arrives the
/// server is already this far ahead. Anchoring the client's clock to the arrival
/// tick instead of the snapshot tick keeps the local solve from resolving against
/// stale peers. No sample (or a path with no measurable RTT) means no bias.
pub fn one_way_ticks(rtt: Duration) -> i32 {
    if rtt.is_zero() {
        return 0;
    }
    let ticks = rtt.as_secs_f64() * 0.5 / proto::CLIENT_TICK_DT as f64;
    ticks.round().clamp(0.0, proto::REPORT_WINDOW as f64) as i32
}

/// Snapshots in, with the link whose measured RTT biases the clock anchor.
type SnapshotIn<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static mut MessageReceiver<proto::ServerSnapshot>>,
        Option<&'static Link>,
    ),
    With<Client>,
>;

/// Reports out, but only while the link is live: lightyear only drains senders on
/// a `Connected` entity, so buffering into a dead one would grow without bound.
type ReportOut<'w, 's> = Query<
    'w,
    's,
    Option<&'static mut MessageSender<proto::PlayerMovementPacket>>,
    (With<Client>, With<Connected>),
>;

fn drain_snapshots(mut local: ResMut<Local>, mut stats: ResMut<NetStats>, mut clients: SnapshotIn) {
    for (receiver, link) in &mut clients {
        // The typed receiver appears once the first payload of that type arrives.
        let Some(mut receiver) = receiver else {
            continue;
        };
        let bias = one_way_ticks(link.map_or(Duration::ZERO, |l| l.stats.rtt));
        for snapshot in receiver.receive() {
            stats.received += 1;
            stats.acked = snapshot.acked_seq;
            stats.unacked = local.seq.wrapping_sub(snapshot.acked_seq);
            if snapshot.resync {
                stats.resyncs += 1;
                info!(
                    "server placed this session at {:?}",
                    snapshot.shadow_position
                );
            }
            if local.accept_snapshot(&snapshot, bias) {
                stats.corrections += 1;
            }
        }
    }
}

/// Surfaces a dead link instead of leaving a client that looks connected.
fn report_disconnects(
    mut stats: ResMut<NetStats>,
    mut links: Query<&Disconnected, Added<Disconnected>>,
) {
    for disconnected in &mut links {
        stats.disconnects += 1;
        error!("link closed: {}", disconnected.reason);
    }
}

fn uplink(
    mut local: ResMut<Local>,
    mut stats: ResMut<NetStats>,
    player: Res<PlayerId>,
    mut clients: ReportOut,
) {
    // `report` owns the send cadence and the redundancy window.
    let Some(packet) = local.report(player.0) else {
        return;
    };
    for sender in &mut clients {
        let Some(mut sender) = sender else {
            continue;
        };
        sender.send::<net::InputChannel>(packet.clone());
        stats.sent += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_way_delay_is_half_the_measured_round_trip() {
        assert_eq!(one_way_ticks(Duration::ZERO), 0, "no sample means no bias");
        // 100 ms round trip = 50 ms one way = three 60 Hz ticks.
        assert_eq!(one_way_ticks(Duration::from_millis(100)), 3);
        // An absurd measurement is clamped to the staleness window instead of
        // pushing the predicted tick out of the history entirely.
        assert_eq!(
            one_way_ticks(Duration::from_secs(30)),
            proto::REPORT_WINDOW as i32
        );
    }
}
