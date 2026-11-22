//! Packet rendering and aggregate statistics.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use proto::{PlayerMovementPacket, ServerSnapshot};
use serde::{Deserialize, Serialize};

use crate::config::PacketFilter;
use crate::message::{self, Decoded, Direction, MessageKey};
use crate::relay::Datagram;
use crate::wire::{Batch, Body, Fragment, Packet, WireError};

#[derive(Debug, Deserialize, Serialize)]
struct Ping {
    id: u16,
}

#[derive(Debug, Deserialize, Serialize)]
struct Pong {
    id: u16,
    tick_diff: u32,
    overstep: u8,
}

#[derive(Debug, Deserialize, Serialize)]
struct UnknownMessage {
    net_id: Option<u16>,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum DirectionFilter {
    Both,
    #[value(name = "c2s")]
    ClientToServer,
    #[value(name = "s2c")]
    ServerToClient,
}

impl DirectionFilter {
    pub fn shows(self, direction: Direction) -> bool {
        matches!(
            (self, direction),
            (Self::Both, _)
                | (Self::ClientToServer, Direction::ClientToServer)
                | (Self::ServerToClient, Direction::ServerToClient)
        )
    }
}

pub struct Options {
    pub detail: bool,
    pub hex: bool,
    pub show_acks: bool,
    pub directions: DirectionFilter,
    pub color: bool,
    pub stats_every: Option<Duration>,
    pub limit: u64,
    pub filter: PacketFilter,
}

pub struct Reporter {
    opts: Options,
    start: Instant,
    last_stats: Instant,
    next_stats: Option<Instant>,
    observed: u64,
    displayed: u64,
    totals: Totals,
    ids: HashMap<(Direction, SocketAddr), PacketIdTracker>,
    rate: RateSnapshot,
}

impl Reporter {
    pub fn new(opts: Options) -> Self {
        let now = Instant::now();
        let next_stats = opts.stats_every.map(|every| now + every);
        Self {
            opts,
            start: now,
            last_stats: now,
            next_stats,
            observed: 0,
            displayed: 0,
            totals: Totals::default(),
            ids: HashMap::new(),
            rate: RateSnapshot::default(),
        }
    }

    /// Returns true once `--count` datagrams have been observed.
    pub fn on_datagram(&mut self, datagram: Datagram<'_>) -> bool {
        if self.limit_reached() {
            // Traffic keeps flowing through the relay; only the report stops.
            return true;
        }
        self.observed += 1;
        let parsed = Packet::parse(datagram.bytes);

        if let Ok(packet) = &parsed {
            let peer = match datagram.direction {
                Direction::ClientToServer => datagram.from,
                Direction::ServerToClient => datagram.to,
            };
            let tracker = self.ids.entry((datagram.direction, peer)).or_default();
            let before = (tracker.gaps, tracker.reordered);
            tracker.observe(packet.header.packet_id);
            let gaps = tracker.gaps - before.0;
            let reordered = tracker.reordered - before.1;
            let stats = self.totals.get_mut(datagram.direction);
            stats.gaps += gaps;
            stats.reordered += reordered;
        }

        let stats = self.totals.get_mut(datagram.direction);
        stats.packets += 1;
        stats.bytes += datagram.bytes.len() as u64;
        let mut ack_only = false;
        match &parsed {
            Ok(packet) => {
                ack_only = matches!(packet.body, Body::AckOnly);
                record_messages(stats, packet, datagram.direction);
            }
            Err(_) => stats.malformed += 1,
        }

        let visible = self.opts.directions.shows(datagram.direction)
            && (self.opts.show_acks || !ack_only)
            && packet_allows_filter(&self.opts.filter, &parsed, datagram.direction);
        if visible {
            self.displayed += 1;
            let elapsed = self.start.elapsed();
            if self.opts.detail {
                self.render_detail(self.displayed, elapsed, &datagram, &parsed);
            } else {
                self.render_line(self.displayed, elapsed, &datagram, &parsed);
            }
        }

        self.opts.limit != 0 && self.observed >= self.opts.limit
    }

    pub fn limit_reached(&self) -> bool {
        self.opts.limit != 0 && self.observed >= self.opts.limit
    }

    pub fn tick(&mut self, now: Instant) {
        let Some(every) = self.opts.stats_every else {
            return;
        };
        if now < self.next_stats.unwrap_or(now) {
            return;
        }
        if self.observed == 0 {
            // Do not fill the log with empty stats before the first client
            // datagram; once traffic has started, zero-rate lines are useful.
            self.next_stats = Some(now + every);
            self.last_stats = now;
            return;
        }
        self.next_stats = Some(now + every);
        self.print_periodic(now);
        self.last_stats = now;
        self.rate = RateSnapshot {
            c2s_packets: self.totals.c2s.packets,
            c2s_bytes: self.totals.c2s.bytes,
            s2c_packets: self.totals.s2c.packets,
            s2c_bytes: self.totals.s2c.bytes,
        };
    }

    pub fn finish(&self) {
        let elapsed = self.start.elapsed().as_secs_f64();
        let total_bytes = self.totals.c2s.bytes + self.totals.s2c.bytes;
        println!();
        println!("{}", self.paint("90", &"-".repeat(100)));
        println!(
            "summary  {elapsed:.3}s  {} datagrams  {}  |  C->S {} pkt {}  S->C {} pkt {}  |  gaps {}  reordered {}  malformed {}",
            self.observed,
            human_bytes(total_bytes),
            self.totals.c2s.packets,
            human_bytes(self.totals.c2s.bytes),
            self.totals.s2c.packets,
            human_bytes(self.totals.s2c.bytes),
            self.totals.c2s.gaps + self.totals.s2c.gaps,
            self.totals.c2s.reordered + self.totals.s2c.reordered,
            self.totals.c2s.malformed + self.totals.s2c.malformed,
        );
        let internal = self.totals.internal_messages();
        if internal > 0 {
            println!(
                "         {internal} internal lightyear messages were not movement packets and were only counted"
            );
        }
    }

    fn render_line(
        &self,
        no: u64,
        elapsed: Duration,
        datagram: &Datagram<'_>,
        parsed: &Result<Packet<'_>, WireError>,
    ) {
        println!("{}", self.format_line(no, elapsed, datagram, parsed));
    }

    fn format_line(
        &self,
        no: u64,
        elapsed: Duration,
        datagram: &Datagram<'_>,
        parsed: &Result<Packet<'_>, WireError>,
    ) -> String {
        let direction = self.paint_dir(datagram.direction);
        let peer = match datagram.direction {
            Direction::ClientToServer => datagram.from.port(),
            Direction::ServerToClient => datagram.to.port(),
        };
        let body = match parsed {
            Ok(packet) => self.body_summary(packet, datagram.direction),
            Err(error) => self.paint("31", &format!("malformed: {error}")),
        };
        match parsed {
            Ok(packet) => format!(
                "#{no:06}  {:>8.3}s  {direction} :{peer:<5}  {:>4} B  pkt={:<5} t={:<8}  {body}",
                elapsed.as_secs_f64(),
                packet.len,
                packet.header.packet_id,
                packet.header.tick,
            ),
            Err(_) => format!(
                "#{no:06}  {:>8.3}s  {direction} :{peer:<5}  {:>4} B  {:<14}  {body}",
                elapsed.as_secs_f64(),
                datagram.bytes.len(),
                "",
            ),
        }
    }

    fn render_detail(
        &self,
        no: u64,
        elapsed: Duration,
        datagram: &Datagram<'_>,
        parsed: &Result<Packet<'_>, WireError>,
    ) {
        println!("{}", self.paint("90", &"-".repeat(100)));
        println!(
            "#{no:06}  {:.3}s  {}  {} B  {} -> {}",
            elapsed.as_secs_f64(),
            self.paint_dir(datagram.direction),
            parsed
                .as_ref()
                .map_or(datagram.bytes.len(), |packet| packet.len),
            datagram.from,
            datagram.to
        );
        match parsed {
            Err(error) => println!("  {} {error}", self.paint("31", "malformed:")),
            Ok(packet) => {
                println!(
                    "  {}  packet={}  tick={}  ack={}  bits=0x{:08x}",
                    packet.header.packet_type.name(),
                    packet.header.packet_id,
                    packet.header.tick,
                    packet.header.last_ack_packet_id,
                    packet.header.ack_bitfield
                );
                match &packet.body {
                    Body::Data(batches) => {
                        for batch in batches {
                            self.print_batch(batch, datagram.direction);
                        }
                    }
                    Body::Fragment {
                        channel,
                        fragment,
                        tail,
                    } => {
                        self.print_fragment(*channel, fragment, datagram.direction);
                        for batch in tail {
                            self.print_batch(batch, datagram.direction);
                        }
                    }
                    Body::AckOnly => println!("  (acknowledgement only)"),
                    Body::Compressed => println!(
                        "  {} {} payload body cannot be decoded (LZ4 is not enabled)",
                        self.paint("31", "!"),
                        packet.header.packet_type.name()
                    ),
                }
            }
        }
        if self.opts.hex {
            self.print_hex(datagram.bytes);
        }
    }

    fn body_summary(&self, packet: &Packet<'_>, direction: Direction) -> String {
        match &packet.body {
            Body::AckOnly => format!(
                "{}last_ack={} bits=0x{:08x}",
                self.kind("35", "ACK"),
                packet.header.last_ack_packet_id,
                packet.header.ack_bitfield
            ),
            Body::Compressed => format!(
                "{}({} body, not decoded)",
                self.kind("33", "COMPRESSED"),
                packet.header.packet_type.name()
            ),
            Body::Fragment {
                channel,
                fragment,
                tail,
            } => {
                let mut out = format!(
                    "{}channel={} msg={} {}/{} {} B",
                    self.kind("33", "FRAGMENT"),
                    channel,
                    fragment.message_id,
                    fragment.index + 1,
                    fragment.count,
                    fragment.payload.len()
                );
                if fragment.index == 0 && tail.is_empty() {
                    let decoded = message::decode(*channel, direction, fragment.payload);
                    if self.opts.filter.allows(decoded.key()) {
                        out.push_str("  ");
                        out.push_str(&self.decoded_summary(decoded));
                    }
                }
                out
            }
            Body::Data(batches) => {
                let mut parts = Vec::new();
                for batch in batches {
                    for message in &batch.messages {
                        let decoded = message::decode(batch.channel, direction, message.payload);
                        if !self.opts.filter.allows(decoded.key()) {
                            continue;
                        }
                        parts.push(self.decoded_summary(decoded));
                    }
                }
                if parts.is_empty() {
                    format!("{}(no messages)", self.kind("37", "DATA"))
                } else {
                    parts.join("  |  ")
                }
            }
        }
    }

    fn decoded_summary(&self, decoded: Decoded) -> String {
        match decoded {
            Decoded::Input { net_id, packet } => self.input_summary(net_id, &packet),
            Decoded::Snapshot { net_id, packet } => self.snapshot_summary(net_id, &packet),
            Decoded::Ping { net_id, id } => self.ping_summary(net_id, id),
            Decoded::Pong {
                net_id,
                id,
                tick_diff,
                overstep,
            } => self.pong_summary(net_id, id, tick_diff, overstep),
            Decoded::Unknown { net_id, bytes } => format!(
                "{}{}",
                self.kind("90", "INTERNAL"),
                ron_compact(&UnknownMessage {
                    net_id,
                    bytes: bytes as u64,
                })
            ),
        }
    }

    fn input_summary(&self, net_id: u16, packet: &PlayerMovementPacket) -> String {
        format!(
            "{}net_id={net_id}  {}",
            self.kind("36", "INPUT"),
            ron_compact(packet)
        )
    }

    fn snapshot_summary(&self, net_id: u16, packet: &ServerSnapshot) -> String {
        format!(
            "{}net_id={net_id}  {}",
            self.kind("32", "SNAPSHOT"),
            ron_compact(packet)
        )
    }

    fn ping_summary(&self, net_id: u16, id: u16) -> String {
        format!(
            "{}net_id={net_id}  {}",
            self.kind("33", "PING"),
            ron_compact(&Ping { id })
        )
    }

    fn pong_summary(&self, net_id: u16, id: u16, tick_diff: u32, overstep: u8) -> String {
        format!(
            "{}net_id={net_id}  {}",
            self.kind("33", "PONG"),
            ron_compact(&Pong {
                id,
                tick_diff,
                overstep,
            })
        )
    }

    fn print_batch(&self, batch: &Batch<'_>, direction: Direction) {
        let name = channel_name(batch, direction);
        println!("  channel {}{}", batch.channel, name);
        for (index, message) in batch.messages.iter().enumerate() {
            let decoded = message::decode(batch.channel, direction, message.payload);
            if !self.opts.filter.allows(decoded.key()) {
                continue;
            }
            let message_id = message
                .id
                .map_or_else(String::new, |id| format!("  msg_id={id}"));
            match decoded {
                Decoded::Input { net_id, packet } => {
                    println!(
                        "    msg {index}  PlayerMovementPacket  net_id={net_id}  {} B{message_id}",
                        message.payload.len()
                    );
                    self.print_ron(&packet);
                }
                Decoded::Snapshot { net_id, packet } => {
                    println!(
                        "    msg {index}  ServerSnapshot  net_id={net_id}  {} B{message_id}",
                        message.payload.len()
                    );
                    self.print_ron(&packet);
                }
                Decoded::Ping { net_id, id } => {
                    println!(
                        "    msg {index}  Ping  net_id={net_id}  {} B{message_id}",
                        message.payload.len()
                    );
                    self.print_ron(&Ping { id });
                }
                Decoded::Pong {
                    net_id,
                    id,
                    tick_diff,
                    overstep,
                } => {
                    println!(
                        "    msg {index}  Pong  net_id={net_id}  {} B{message_id}",
                        message.payload.len()
                    );
                    self.print_ron(&Pong {
                        id,
                        tick_diff,
                        overstep,
                    });
                }
                Decoded::Unknown { net_id, bytes } => {
                    println!("    msg {index}  internal  (not decoded)");
                    self.print_ron(&UnknownMessage {
                        net_id,
                        bytes: bytes as u64,
                    });
                }
            }
        }
    }

    fn print_ron<T: Serialize>(&self, value: &T) {
        for line in ron_pretty(value).lines() {
            println!("      {line}");
        }
    }

    fn print_fragment(&self, channel: u16, fragment: &Fragment<'_>, direction: Direction) {
        println!(
            "  fragment  channel={channel}  message_id={}  {}/{}  compression={}  {} B",
            fragment.message_id,
            fragment.index + 1,
            fragment.count,
            match fragment.compression {
                Some(0) => "none",
                Some(1) => "lz4",
                Some(_) => "unknown",
                None => "-",
            },
            fragment.payload.len()
        );
        if fragment.index == 0 {
            let decoded = message::decode(channel, direction, fragment.payload);
            if !self.opts.filter.allows(decoded.key()) {
                return;
            }
            match decoded {
                Decoded::Input { packet, .. } => self.print_ron(&packet),
                Decoded::Snapshot { packet, .. } => self.print_ron(&packet),
                Decoded::Ping { id, .. } => self.print_ron(&Ping { id }),
                Decoded::Pong {
                    id,
                    tick_diff,
                    overstep,
                    ..
                } => self.print_ron(&Pong {
                    id,
                    tick_diff,
                    overstep,
                }),
                Decoded::Unknown { .. } => {
                    println!("    first fragment does not contain a whole known message")
                }
            }
        }
    }

    fn print_hex(&self, bytes: &[u8]) {
        for (line, chunk) in bytes.chunks(16).enumerate() {
            let mut hex = String::with_capacity(chunk.len() * 3);
            for byte in chunk {
                hex.push_str(&format!("{byte:02x} "));
            }
            println!("    {:04x}  {}", line * 16, hex.trim_end());
        }
    }

    fn print_periodic(&self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_stats)
            .as_secs_f64()
            .max(1e-9);
        let c2s = &self.totals.c2s;
        let s2c = &self.totals.s2c;
        let c2s_packets = c2s.packets.saturating_sub(self.rate.c2s_packets);
        let c2s_bytes = c2s.bytes.saturating_sub(self.rate.c2s_bytes);
        let s2c_packets = s2c.packets.saturating_sub(self.rate.s2c_packets);
        let s2c_bytes = s2c.bytes.saturating_sub(self.rate.s2c_bytes);

        println!(
            "stats {:>6.2}s  |  C->S {:>5} pkt {:>10}  {:>6.1} pkt/s {:>10}  |  {}  |  gaps {}  reorder {}  bad {}",
            elapsed,
            c2s.packets,
            human_bytes(c2s.bytes),
            c2s_packets as f64 / elapsed,
            human_rate(c2s_bytes as f64 / elapsed),
            messages_summary(c2s),
            c2s.gaps,
            c2s.reordered,
            c2s.malformed
        );
        println!(
            "             |  S->C {:>5} pkt {:>10}  {:>6.1} pkt/s {:>10}  |  {}  |  gaps {}  reorder {}  bad {}",
            s2c.packets,
            human_bytes(s2c.bytes),
            s2c_packets as f64 / elapsed,
            human_rate(s2c_bytes as f64 / elapsed),
            messages_summary(s2c),
            s2c.gaps,
            s2c.reordered,
            s2c.malformed
        );
    }

    fn paint_dir(&self, direction: Direction) -> String {
        match direction {
            Direction::ClientToServer => self.paint("36", direction.label()),
            Direction::ServerToClient => self.paint("32", direction.label()),
        }
    }

    fn kind(&self, color: &str, name: &str) -> String {
        self.paint(color, &format!("{name:<8} "))
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.opts.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

#[derive(Default)]
struct Totals {
    c2s: DirStats,
    s2c: DirStats,
}

impl Totals {
    fn get_mut(&mut self, direction: Direction) -> &mut DirStats {
        match direction {
            Direction::ClientToServer => &mut self.c2s,
            Direction::ServerToClient => &mut self.s2c,
        }
    }

    fn internal_messages(&self) -> u64 {
        self.c2s
            .messages
            .get("INTERNAL")
            .map_or(0, |stat| stat.count)
            + self
                .s2c
                .messages
                .get("INTERNAL")
                .map_or(0, |stat| stat.count)
    }
}

#[derive(Default)]
struct DirStats {
    packets: u64,
    bytes: u64,
    messages: BTreeMap<&'static str, MessageStat>,
    gaps: u64,
    reordered: u64,
    malformed: u64,
}

#[derive(Clone, Copy, Default)]
struct MessageStat {
    count: u64,
    bytes: u64,
}

#[derive(Default)]
struct PacketIdTracker {
    last: Option<u16>,
    gaps: u64,
    reordered: u64,
}

impl PacketIdTracker {
    fn observe(&mut self, id: u16) {
        match self.last {
            None => self.last = Some(id),
            Some(last) => {
                let delta = id.wrapping_sub(last) as i16;
                if delta > 0 {
                    self.gaps += u64::from(delta as u16) - 1;
                    self.last = Some(id);
                } else {
                    // Duplicate or reordered; the high-water mark stays put.
                    self.reordered += 1;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct RateSnapshot {
    c2s_packets: u64,
    c2s_bytes: u64,
    s2c_packets: u64,
    s2c_bytes: u64,
}

/// Calls `f` once per message whose type this datagram reveals. Fragment
/// continuations and compressed bodies are not decoded and are skipped.
fn for_each_message(packet: &Packet<'_>, direction: Direction, mut f: impl FnMut(Decoded, usize)) {
    match &packet.body {
        Body::Data(batches) => {
            for batch in batches {
                for message in &batch.messages {
                    f(
                        message::decode(batch.channel, direction, message.payload),
                        message.payload.len(),
                    );
                }
            }
        }
        Body::Fragment {
            channel,
            fragment,
            tail,
        } => {
            if fragment.index == 0 {
                f(
                    message::decode(*channel, direction, fragment.payload),
                    fragment.payload.len(),
                );
            }
            for batch in tail {
                for message in &batch.messages {
                    f(
                        message::decode(batch.channel, direction, message.payload),
                        message.payload.len(),
                    );
                }
            }
        }
        Body::AckOnly | Body::Compressed => {}
    }
}

/// A datagram is printed when any message inside it survives the filter.
/// Messages whose type is not in the datagram (an ACK, a continuation
/// fragment, a compressed or malformed body) are matched as their own key.
fn packet_allows_filter(
    filter: &PacketFilter,
    parsed: &Result<Packet<'_>, WireError>,
    direction: Direction,
) -> bool {
    let Ok(packet) = parsed else {
        return filter.allows(MessageKey::internal());
    };
    match &packet.body {
        Body::AckOnly => filter.allows(MessageKey::ack()),
        Body::Compressed => filter.allows(MessageKey::internal()),
        Body::Data(_) | Body::Fragment { .. } => {
            let mut seen = false;
            let mut allowed = false;
            for_each_message(packet, direction, |decoded, _| {
                seen = true;
                allowed |= filter.allows(decoded.key());
            });
            if seen {
                allowed
            } else {
                // A continuation fragment or a Data packet without batches.
                filter.allows(MessageKey::internal())
            }
        }
    }
}

fn record_messages(stats: &mut DirStats, packet: &Packet<'_>, direction: Direction) {
    for_each_message(packet, direction, |decoded, bytes| {
        let stat = stats.messages.entry(decoded.key().kind.name()).or_default();
        stat.count += 1;
        stat.bytes += bytes as u64;
    });
}

fn channel_name(batch: &Batch<'_>, direction: Direction) -> String {
    for message in &batch.messages {
        match message::decode(batch.channel, direction, message.payload) {
            Decoded::Input { .. } => return "  (InputChannel)".to_string(),
            Decoded::Snapshot { .. } => return "  (SnapshotChannel)".to_string(),
            Decoded::Ping { .. } | Decoded::Pong { .. } => return "  (PingChannel)".to_string(),
            Decoded::Unknown { .. } => {}
        }
    }
    "  (internal)".to_string()
}

fn ron_compact<T: ?Sized + Serialize>(value: &T) -> String {
    let config = ron::ser::PrettyConfig::new()
        .struct_names(true)
        .compact_structs(true)
        .compact_arrays(true)
        .compact_maps(true)
        .new_line("")
        .indentor("")
        .separator(" ");
    ron::ser::to_string_pretty(value, config)
        .unwrap_or_else(|error| format!("<RON error: {error}>"))
}

fn ron_pretty<T: ?Sized + Serialize>(value: &T) -> String {
    let config = ron::ser::PrettyConfig::new()
        .struct_names(true)
        .indentor("  ");
    ron::ser::to_string_pretty(value, config)
        .unwrap_or_else(|error| format!("<RON error: {error}>"))
}

fn messages_summary(stats: &DirStats) -> String {
    if stats.messages.is_empty() {
        return "-".to_string();
    }
    stats
        .messages
        .iter()
        .map(|(name, stat)| format!("{name} {}/{}", stat.count, human_bytes(stat.bytes)))
        .collect::<Vec<_>>()
        .join("  ")
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        format!("{bytes:.0} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{:.1} MiB", bytes / MIB)
    }
}

fn human_rate(bytes_per_second: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes_per_second < KIB {
        format!("{bytes_per_second:.0} B/s")
    } else if bytes_per_second < MIB {
        format!("{:.1} KiB/s", bytes_per_second / KIB)
    } else {
        format!("{:.1} MiB/s", bytes_per_second / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_id_tracker_counts_gaps_and_reordering_across_wrap() {
        let mut tracker = PacketIdTracker::default();
        tracker.observe(10);
        tracker.observe(11);
        tracker.observe(14);
        assert_eq!(tracker.gaps, 2);
        assert_eq!(tracker.reordered, 0);

        tracker.observe(12); // late
        assert_eq!(tracker.reordered, 1);

        tracker.observe(u16::MAX);
        tracker.observe(0);
        tracker.observe(1);
        assert_eq!(tracker.gaps, 2, "wrap counted as a gap");
    }

    #[test]
    fn human_sizes_use_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_rate(512.0), "512 B/s");
        assert_eq!(human_rate(2048.0), "2.0 KiB/s");
    }

    #[test]
    fn direction_filter_hides_only_the_other_side() {
        assert!(DirectionFilter::Both.shows(Direction::ClientToServer));
        assert!(DirectionFilter::Both.shows(Direction::ServerToClient));
        assert!(DirectionFilter::ClientToServer.shows(Direction::ClientToServer));
        assert!(!DirectionFilter::ClientToServer.shows(Direction::ServerToClient));
        assert!(DirectionFilter::ServerToClient.shows(Direction::ServerToClient));
        assert!(!DirectionFilter::ServerToClient.shows(Direction::ClientToServer));
    }

    fn sample_input() -> PlayerMovementPacket {
        PlayerMovementPacket {
            seq: 77,
            player: 1,
            report: proto::MovementReport {
                tick: proto::Tick::new(90),
                position: [0.0, 0.9, -1.5],
                linear_velocity: [0.0, 0.0, -5.5],
                angular_velocity: [0.0, 0.0, 0.0],
                body_yaw: 0.0,
                grounded: true,
                applied_correction_seq: 3,
            },
            inputs: vec![proto::InputFrame {
                tick: proto::Tick::new(90),
                move_x: 0,
                move_z: -127,
                buttons: proto::buttons::JUMP,
                camera_yaw: proto::quantise_yaw(0.0),
            }],
        }
    }

    fn sample_snapshot() -> ServerSnapshot {
        ServerSnapshot {
            server_tick: proto::Tick::new(90),
            acked_seq: 3,
            shadow_position: [0.0, 0.9, -1.5],
            correction: Some(proto::CollisionCorrection {
                seq: 7,
                tick: proto::Tick::new(89),
                epoch: 1,
                velocity_delta: [0.0, 0.0, -0.5],
                hard_normal: Some([0.0, 1.0, 0.0]),
            }),
            peers: std::iter::once(proto::PeerCollider {
                player: 2,
                position: [0.1, 0.2, 0.3],
                body_yaw: 0.5,
                mode: proto::peer_mode::SOFT,
            })
            .collect(),
            resync: false,
        }
    }

    #[test]
    fn movement_content_roundtrips_as_compact_named_ron() {
        let input = sample_input();
        let text = ron_compact(&input);
        assert!(text.starts_with("PlayerMovementPacket("), "{text}");
        assert!(text.contains("seq: 77"), "{text}");
        assert!(text.contains("inputs: [InputFrame("), "{text}");
        assert!(!text.contains('\n'), "{text}");

        let decoded: PlayerMovementPacket = ron::from_str(&text).expect("RON roundtrip");
        assert_eq!(decoded.seq, input.seq);
        assert_eq!(decoded.player, input.player);
        assert_eq!(decoded.inputs.len(), 1);
    }

    #[test]
    fn pretty_ron_is_multiline_and_named() {
        let text = ron_pretty(&sample_input());
        assert!(text.contains("PlayerMovementPacket("), "{text}");
        assert!(text.contains("\n  seq: 77"), "{text}");
        assert!(text.lines().count() > 3, "{text}");
    }

    #[test]
    fn snapshot_content_roundtrips_as_ron() {
        let snapshot = sample_snapshot();
        let text = ron_compact(&snapshot);
        assert!(text.starts_with("ServerSnapshot("), "{text}");
        let decoded: ServerSnapshot = ron::from_str(&text).expect("RON roundtrip");
        assert_eq!(decoded.server_tick.0, 90);
        assert_eq!(decoded.peers.len(), 1);
        assert!(decoded.correction.is_some());
    }

    #[test]
    fn control_content_roundtrips_as_ron() {
        let ping = ron_compact(&Ping { id: 3 });
        assert!(ping.starts_with("Ping("), "{ping}");
        let decoded: Ping = ron::from_str(&ping).expect("ping RON");
        assert_eq!(decoded.id, 3);

        let pong = ron_compact(&Pong {
            id: 3,
            tick_diff: 7,
            overstep: 1,
        });
        assert!(pong.starts_with("Pong("), "{pong}");
        let decoded: Pong = ron::from_str(&pong).expect("pong RON");
        assert_eq!((decoded.id, decoded.tick_diff, decoded.overstep), (3, 7, 1));
    }

    #[test]
    fn one_line_output_embeds_the_ron_payload() {
        fn put_varint(out: &mut Vec<u8>, value: u64) {
            if value <= 63 {
                out.push(value as u8);
            } else if value <= 16_383 {
                out.extend_from_slice(&((value as u16) | 0x4000).to_be_bytes());
            } else if value <= 1_073_741_823 {
                out.extend_from_slice(&((value as u32) | 0x8000_0000).to_be_bytes());
            } else {
                out.extend_from_slice(&(value | 0xc000_0000_0000_0000).to_be_bytes());
            }
        }

        let mut payload = vec![2u8]; // PlayerMovementPacket net id
        payload.extend_from_slice(&postcard::to_allocvec(&sample_input()).unwrap());

        let mut bytes = vec![0u8]; // Data packet
        bytes.extend_from_slice(&9u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&5u32.to_be_bytes());
        put_varint(&mut bytes, 1); // channel
        bytes.push(1); // one message
        bytes.push(0); // no message id
        put_varint(&mut bytes, payload.len() as u64);
        bytes.extend_from_slice(&payload);

        let datagram = Datagram {
            direction: Direction::ClientToServer,
            from: "127.0.0.1:54321".parse().unwrap(),
            to: "127.0.0.1:4000".parse().unwrap(),
            bytes: &bytes,
        };
        let parsed = Packet::parse(datagram.bytes);
        let reporter = Reporter::new(Options {
            detail: false,
            hex: false,
            show_acks: true,
            directions: DirectionFilter::Both,
            color: false,
            stats_every: None,
            limit: 0,
            filter: PacketFilter::default(),
        });

        let line = reporter.format_line(8, Duration::from_millis(1500), &datagram, &parsed);
        assert!(line.contains(":54321"), "{line}");
        assert!(line.contains("net_id=2"), "{line}");
        assert!(line.contains("PlayerMovementPacket("), "{line}");
        assert!(line.contains("seq: 77"), "{line}");
    }

    #[test]
    fn one_line_output_names_the_direction_and_client_port() {
        let mut bytes = vec![4u8]; // AckOnly
        bytes.extend_from_slice(&9u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&5u32.to_be_bytes());

        let datagram = Datagram {
            direction: Direction::ServerToClient,
            from: "127.0.0.1:4000".parse().unwrap(),
            to: "127.0.0.1:54321".parse().unwrap(),
            bytes: &bytes,
        };
        let parsed = Packet::parse(datagram.bytes);
        let reporter = Reporter::new(Options {
            detail: false,
            hex: false,
            show_acks: true,
            directions: DirectionFilter::Both,
            color: false,
            stats_every: None,
            limit: 0,
            filter: PacketFilter::default(),
        });

        let line = reporter.format_line(7, Duration::from_millis(1234), &datagram, &parsed);
        assert!(line.contains("S->C"), "{line}");
        assert!(line.contains(":54321"), "{line}");
        assert!(line.contains("pkt=9"), "{line}");
        assert!(line.contains("ACK"), "{line}");
    }

    fn filter(entries: &[&str]) -> PacketFilter {
        let entries: Vec<String> = entries.iter().map(|entry| entry.to_string()).collect();
        PacketFilter::parse(&entries).expect("valid filter")
    }

    fn data_datagram(payloads: &[&[u8]], channel: u16) -> Vec<u8> {
        let mut bytes = vec![0u8]; // Data packet
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&7u32.to_be_bytes());
        bytes.push(channel as u8);
        bytes.push(payloads.len() as u8);
        for payload in payloads {
            bytes.push(0); // no message id
            bytes.push(payload.len() as u8);
            bytes.extend_from_slice(payload);
        }
        bytes
    }

    #[test]
    fn the_display_filter_matches_any_message_in_a_datagram() {
        let ping = data_datagram(&[&[0x00, 0x12, 0x34]], 0);
        let parsed = Packet::parse(&ping);

        let hide_ping = filter(&["!ping"]);
        assert!(!packet_allows_filter(
            &hide_ping,
            &parsed,
            Direction::ClientToServer
        ));
        let show_ping = filter(&["ping"]);
        assert!(packet_allows_filter(
            &show_ping,
            &parsed,
            Direction::ClientToServer
        ));

        // A mixed datagram stays visible while any message matches.
        let mixed = data_datagram(&[&[0x00, 0x12, 0x34], &[4, 0xff, 0x00]], 0);
        let mixed = Packet::parse(&mixed);
        assert!(packet_allows_filter(
            &filter(&["!ping"]),
            &mixed,
            Direction::ClientToServer
        ));
        assert!(!packet_allows_filter(
            &filter(&["!ping", "!internal"]),
            &mixed,
            Direction::ClientToServer
        ));

        // An ACK carries no messages and only matches the `ack` name.
        let mut ack = vec![4u8];
        ack.extend_from_slice(&1u16.to_be_bytes());
        ack.extend_from_slice(&0u16.to_be_bytes());
        ack.extend_from_slice(&0u32.to_be_bytes());
        ack.extend_from_slice(&7u32.to_be_bytes());
        let ack = Packet::parse(&ack);
        assert!(packet_allows_filter(
            &hide_ping,
            &ack,
            Direction::ClientToServer
        ));
        assert!(!packet_allows_filter(
            &show_ping,
            &ack,
            Direction::ClientToServer
        ));
    }
}
