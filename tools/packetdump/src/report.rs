//! Packet rendering and aggregate statistics.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::PacketFilter;
use crate::message::{self, Decoded, Direction, MessageKey, PlayerInput};
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
    /// Widest line the periodic statistics may print.
    width: usize,
}

/// Default width for the periodic statistics when `COLUMNS` is not set.
const DEFAULT_STATS_WIDTH: usize = 100;

/// The terminal width, from `COLUMNS` when the shell exports it. A very narrow
/// value is ignored: wrapping every field is harder to read than a slightly
/// long line.
fn stats_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.trim().parse::<usize>().ok())
        .filter(|width| *width >= 40)
        .unwrap_or(DEFAULT_STATS_WIDTH)
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
            width: stats_width(),
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
                record_messages(stats, packet);
            }
            Err(_) => stats.malformed += 1,
        }

        let visible = self.opts.directions.shows(datagram.direction)
            && (self.opts.show_acks || !ack_only)
            && packet_allows_filter(&self.opts.filter, &parsed);
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
            "summary  {elapsed:.3}s  {} datagrams  {}",
            self.observed,
            human_bytes(total_bytes),
        );
        println!(
            "         C->S {} pkt {}   S->C {} pkt {}",
            self.totals.c2s.packets,
            human_bytes(self.totals.c2s.bytes),
            self.totals.s2c.packets,
            human_bytes(self.totals.s2c.bytes),
        );
        println!(
            "         gaps {}  reordered {}  malformed {}",
            self.totals.c2s.gaps + self.totals.s2c.gaps,
            self.totals.c2s.reordered + self.totals.s2c.reordered,
            self.totals.c2s.malformed + self.totals.s2c.malformed,
        );
        let internal = self.totals.internal_messages();
        if internal > 0 {
            println!("         {internal} messages were not decoded and were only counted");
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
        match parsed {
            Ok(packet) => {
                let head = format!(
                    "#{no:06}  {:>8.3}s  {direction} :{peer:<5} {:>4} B  pkt={}  t={}",
                    elapsed.as_secs_f64(),
                    packet.len,
                    packet.header.packet_id,
                    packet.header.tick,
                );
                join_parts(head, self.body_parts(packet))
            }
            Err(error) => format!(
                "#{no:06}  {:>8.3}s  {direction} :{peer:<5} {:>4} B  {:<20}  {}",
                elapsed.as_secs_f64(),
                datagram.bytes.len(),
                "",
                self.paint("31", &format!("malformed: {error}")),
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
                            self.print_batch(batch);
                        }
                    }
                    Body::Fragment {
                        channel,
                        fragment,
                        tail,
                    } => {
                        self.print_fragment(*channel, fragment);
                        for batch in tail {
                            self.print_batch(batch);
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

    /// One summary per displayed message in the datagram. Several messages can
    /// share a datagram, and their summaries would overrun a terminal if they
    /// were joined on one line, so the caller prints each on its own line.
    fn body_parts(&self, packet: &Packet<'_>) -> Vec<String> {
        match &packet.body {
            Body::AckOnly => vec![format!(
                "{}last_ack={} bits=0x{:08x}",
                self.kind("35", "ACK"),
                packet.header.last_ack_packet_id,
                packet.header.ack_bitfield
            )],
            Body::Compressed => vec![format!(
                "{}({} body, not decoded)",
                self.kind("33", "COMPRESSED"),
                packet.header.packet_type.name()
            )],
            Body::Fragment {
                channel,
                fragment,
                tail,
            } => {
                let mut parts = vec![format!(
                    "{}channel={} msg={} {}/{} {} B",
                    self.kind("33", "FRAGMENT"),
                    channel,
                    fragment.message_id,
                    fragment.index + 1,
                    fragment.count,
                    fragment.payload.len()
                )];
                if fragment.index == 0 && tail.is_empty() {
                    let decoded = message::decode(*channel, fragment.payload);
                    if self.opts.filter.allows(decoded.key()) {
                        parts.push(self.decoded_summary(decoded));
                    }
                }
                parts
            }
            Body::Data(batches) => {
                let mut parts = Vec::new();
                for batch in batches {
                    for message in &batch.messages {
                        let decoded = message::decode(batch.channel, message.payload);
                        if !self.opts.filter.allows(decoded.key()) {
                            continue;
                        }
                        parts.push(self.decoded_summary(decoded));
                    }
                }
                if parts.is_empty() {
                    vec![format!("{}(no messages)", self.kind("37", "DATA"))]
                } else {
                    parts
                }
            }
        }
    }

    fn decoded_summary(&self, decoded: Decoded) -> String {
        match decoded {
            Decoded::Input { net_id, message } => self.input_summary(net_id, &message),
            Decoded::ProtocolCheck { net_id, .. } => format!(
                // The three checksums are 20-digit numbers; the one-line view
                // keeps them for `--detail` only so the line stays readable.
                "{}net_id={net_id}  protocol check",
                self.kind("35", "METADATA")
            ),
            Decoded::SenderMetadata { net_id, bytes } => format!(
                "{}net_id={net_id}  sender metadata, {bytes} B",
                self.kind("35", "METADATA")
            ),
            Decoded::Replication { channel, bytes } => format!(
                "{}{}  {} B",
                self.kind("34", "REPLICATION"),
                channel.name(),
                bytes
            ),
            Decoded::Ping { net_id, id } => self.ping_summary(net_id, id),
            Decoded::Pong {
                net_id,
                id,
                tick_diff,
                overstep,
            } => self.pong_summary(net_id, id, tick_diff, overstep),
            Decoded::Unknown { net_id, bytes } => self.unknown_summary(net_id, bytes),
        }
    }

    fn unknown_summary(&self, net_id: Option<u16>, bytes: usize) -> String {
        match net_id {
            Some(net_id) => format!("{}net_id={net_id}  {bytes} B", self.kind("90", "INTERNAL")),
            None => format!("{}{bytes} B, no message id", self.kind("90", "INTERNAL")),
        }
    }

    fn input_summary(&self, net_id: u16, message: &PlayerInput) -> String {
        format!(
            "{}net_id={net_id}  end_tick={}  targets={}{}",
            self.kind("36", "INPUT"),
            message.end_tick.0,
            message.inputs.len(),
            if message.rebroadcast {
                "  rebroadcast"
            } else {
                ""
            }
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

    fn print_batch(&self, batch: &Batch<'_>) {
        println!("  channel {}{}", batch.channel, channel_name(batch.channel));
        for (index, message) in batch.messages.iter().enumerate() {
            let decoded = message::decode(batch.channel, message.payload);
            if !self.opts.filter.allows(decoded.key()) {
                continue;
            }
            let message_id = message
                .id
                .map_or_else(String::new, |id| format!("  msg_id={id}"));
            let size = message.payload.len();
            match decoded {
                Decoded::Input {
                    net_id,
                    message: input,
                } => {
                    println!(
                        "    msg {index}  InputMessage  net_id={net_id}  {size} B{message_id}"
                    );
                    self.print_ron(input.as_ref());
                }
                Decoded::ProtocolCheck { net_id, hashes } => {
                    println!(
                        "    msg {index}  ProtocolCheck  net_id={net_id}  {size} B{message_id}"
                    );
                    self.print_ron(&hashes);
                }
                Decoded::SenderMetadata { net_id, .. } => {
                    println!(
                        "    msg {index}  SenderMetadata  net_id={net_id}  {size} B{message_id}"
                    );
                    println!("      (lightyear's ToBytes layout, not interpreted)");
                }
                Decoded::Replication { channel, bytes } => {
                    println!(
                        "    msg {index}  replicon {}  {bytes} B{message_id}",
                        channel.name()
                    );
                }
                Decoded::Ping { net_id, id } => {
                    println!("    msg {index}  Ping  net_id={net_id}  {size} B{message_id}");
                    self.print_ron(&Ping { id });
                }
                Decoded::Pong {
                    net_id,
                    id,
                    tick_diff,
                    overstep,
                } => {
                    println!("    msg {index}  Pong  net_id={net_id}  {size} B{message_id}");
                    self.print_ron(&Pong {
                        id,
                        tick_diff,
                        overstep,
                    });
                }
                Decoded::Unknown { net_id, bytes } => {
                    println!("    msg {index}  internal  (not decoded)");
                    println!("      net_id={net_id:?}  {bytes} B");
                }
            }
        }
    }

    fn print_ron<T: Serialize>(&self, value: &T) {
        for line in ron_pretty(value).lines() {
            println!("      {line}");
        }
    }

    fn print_fragment(&self, channel: u16, fragment: &Fragment<'_>) {
        println!(
            "  fragment  channel={channel}{}  message_id={}  {}/{}  compression={}  {} B",
            channel_name(channel),
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
            let decoded = message::decode(channel, fragment.payload);
            if !self.opts.filter.allows(decoded.key()) {
                return;
            }
            match decoded {
                Decoded::Input { message, .. } => self.print_ron(message.as_ref()),
                Decoded::ProtocolCheck { hashes, .. } => self.print_ron(&hashes),
                Decoded::SenderMetadata { .. } => {
                    println!("    (lightyear's ToBytes layout, not interpreted)")
                }
                Decoded::Replication { channel, bytes } => {
                    println!("    replicon {}  {bytes} B", channel.name())
                }
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
        for line in self.stats_block(elapsed, self.width) {
            println!("{line}");
        }
    }

    /// One statistics block, laid out in lines no wider than `width`:
    /// a header, then a rates line and the per-kind breakdown per direction.
    fn stats_block(&self, elapsed: f64, width: usize) -> Vec<String> {
        let mut lines = vec![format!("stats {elapsed:>6.2}s")];
        for (label, stats, packets, bytes) in [
            (
                "C->S",
                &self.totals.c2s,
                self.totals
                    .c2s
                    .packets
                    .saturating_sub(self.rate.c2s_packets),
                self.totals.c2s.bytes.saturating_sub(self.rate.c2s_bytes),
            ),
            (
                "S->C",
                &self.totals.s2c,
                self.totals
                    .s2c
                    .packets
                    .saturating_sub(self.rate.s2c_packets),
                self.totals.s2c.bytes.saturating_sub(self.rate.s2c_bytes),
            ),
        ] {
            lines.push(format!(
                "  {label}  {:>5} pkt  {:>10}  {:>7.1} pkt/s  {:>10}  gaps {}  reorder {}  bad {}",
                stats.packets,
                human_bytes(stats.bytes),
                packets as f64 / elapsed,
                human_rate(bytes as f64 / elapsed),
                stats.gaps,
                stats.reordered,
                stats.malformed,
            ));
            lines.extend(wrap_entries("         ", &message_entries(stats), width));
        }
        lines
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
fn for_each_message(packet: &Packet<'_>, mut f: impl FnMut(Decoded, usize)) {
    match &packet.body {
        Body::Data(batches) => {
            for batch in batches {
                for message in &batch.messages {
                    f(
                        message::decode(batch.channel, message.payload),
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
                    message::decode(*channel, fragment.payload),
                    fragment.payload.len(),
                );
            }
            for batch in tail {
                for message in &batch.messages {
                    f(
                        message::decode(batch.channel, message.payload),
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
fn packet_allows_filter(filter: &PacketFilter, parsed: &Result<Packet<'_>, WireError>) -> bool {
    let Ok(packet) = parsed else {
        return filter.allows(MessageKey::internal());
    };
    match &packet.body {
        Body::AckOnly => filter.allows(MessageKey::ack()),
        Body::Compressed => filter.allows(MessageKey::internal()),
        Body::Data(_) | Body::Fragment { .. } => {
            let mut seen = false;
            let mut allowed = false;
            for_each_message(packet, |decoded, _| {
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

fn record_messages(stats: &mut DirStats, packet: &Packet<'_>) {
    for_each_message(packet, |decoded, bytes| {
        let stat = stats.messages.entry(decoded.key().kind.name()).or_default();
        stat.count += 1;
        stat.bytes += bytes as u64;
    });
}

/// The lightyear channel names, in `ChannelRegistry` order.
fn channel_name(channel: u16) -> &'static str {
    match channel {
        0 => "  (PingChannel)",
        1 => "  (MetadataChannel)",
        2 => "  (RepliconUpdatesChannel)",
        3 => "  (RepliconMutationsChannel)",
        4 => "  (RepliconMutationAcksChannel)",
        5 => "  (InputChannel)",
        _ => "  (unknown)",
    }
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

/// Joins a packet header with its message summaries: the first stays on the
/// header line, the rest continue on their own indented lines so a datagram
/// carrying several messages never overruns the terminal.
fn join_parts(head: String, parts: Vec<String>) -> String {
    let mut out = head;
    for (index, part) in parts.iter().enumerate() {
        out.push_str(if index == 0 { "  " } else { "\n    " });
        out.push_str(part);
    }
    out
}

/// One entry per message kind seen in a direction, `KIND count/size`.
fn message_entries(stats: &DirStats) -> Vec<String> {
    if stats.messages.is_empty() {
        return vec!["-".to_string()];
    }
    stats
        .messages
        .iter()
        .map(|(name, stat)| format!("{name} {}/{}", stat.count, human_bytes(stat.bytes)))
        .collect()
}

/// Packs `entries` into lines no wider than `width`. The first line starts at
/// column 0; continuation lines are indented with `indent`.
fn wrap_entries(indent: &str, entries: &[String], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = indent.to_string();
    let mut has_entry = false;
    for entry in entries {
        let extra = if has_entry {
            entry.len() + 2
        } else {
            entry.len()
        };
        if has_entry && current.len() + extra > width {
            lines.push(std::mem::replace(&mut current, indent.to_string()));
            has_entry = false;
        }
        if has_entry {
            current.push_str("  ");
        }
        current.push_str(entry);
        has_entry = true;
    }
    if has_entry {
        lines.push(current);
    }
    lines
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
    use crate::message::ProtocolHashes;

    #[test]
    fn wrap_entries_keeps_each_line_within_the_width() {
        let entries: Vec<String> = (0..6).map(|i| format!("KIND{i} 1/2 B")).collect();
        let lines = wrap_entries("    ", &entries, 30);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(line.len() <= 30, "{} chars: {line}", line.len());
        }
        assert!(
            lines[1].starts_with("    KIND"),
            "continuation lines are indented: {lines:?}"
        );
    }

    fn reporter() -> Reporter {
        Reporter::new(Options {
            detail: false,
            hex: false,
            show_acks: true,
            directions: DirectionFilter::Both,
            color: false,
            stats_every: None,
            limit: 0,
            filter: PacketFilter::default(),
        })
    }

    fn stats_reporter() -> Reporter {
        let mut reporter = reporter();
        reporter.totals.c2s.packets = 942;
        reporter.totals.c2s.bytes = 106_803;
        reporter.totals.c2s.gaps = 1;
        for (name, count, bytes) in [
            ("INPUT", 935u64, 89_497u64),
            ("METADATA", 2, 16),
            ("PING", 17, 51),
            ("PONG", 16, 128),
            ("REPLICATION", 16, 34),
        ] {
            reporter
                .totals
                .c2s
                .messages
                .insert(name, MessageStat { count, bytes });
        }
        reporter
    }

    #[test]
    fn the_stats_block_wraps_instead_of_overrunning_the_terminal() {
        let lines = stats_reporter().stats_block(1.0, 100);
        for line in &lines {
            assert!(line.len() <= 100, "{} chars: {line}", line.len());
        }
        assert!(lines[0].starts_with("stats "), "{:?}", lines[0]);
        assert!(lines[1].contains("C->S"), "{:?}", lines[1]);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("REPLICATION 16/34 B")),
            "every kind is still reported: {lines:?}"
        );
    }

    #[test]
    fn a_protocol_check_line_stays_within_the_stats_width() {
        let payload = [
            0x02u8, 0x01, 0xed, 0xbd, 0xd2, 0x84, 0xc6, 0x82, 0xb1, 0xc0, 0x03, 0x01, 0xb9, 0xb9,
            0xbf, 0xab, 0xb2, 0xc8, 0xb2, 0x90, 0xc9, 0x01, 0x01, 0xfa, 0xae, 0x83, 0x96, 0xa0,
            0xe5, 0x91, 0xc8, 0x38,
        ];
        let bytes = data_datagram(&[&payload], 1);
        let datagram = Datagram {
            direction: Direction::ServerToClient,
            from: "127.0.0.1:4000".parse().unwrap(),
            to: "127.0.0.1:54321".parse().unwrap(),
            bytes: &bytes,
        };
        let parsed = Packet::parse(datagram.bytes);
        let line = reporter().format_line(2, Duration::from_millis(1594), &datagram, &parsed);
        assert!(line.contains("protocol check"), "{line}");
        assert!(line.len() <= 100, "{} chars: {line}", line.len());
    }

    #[test]
    fn a_datagram_with_several_messages_puts_each_on_its_own_line() {
        // Joining both summaries would push the line past the terminal width.
        let bytes = data_datagram(
            &[&[0x00, 0x11, 0x22], &[0x04, 0x00, 0xb1, 0x01, 0x00, 0x00]],
            5,
        );
        let datagram = Datagram {
            direction: Direction::ClientToServer,
            from: "127.0.0.1:54321".parse().unwrap(),
            to: "127.0.0.1:4000".parse().unwrap(),
            bytes: &bytes,
        };
        let parsed = Packet::parse(datagram.bytes);
        let line = reporter().format_line(584, Duration::from_millis(2008), &datagram, &parsed);

        let lines: Vec<&str> = line.lines().collect();
        assert_eq!(lines.len(), 2, "{line}");
        for line in &lines {
            assert!(line.len() <= 100, "{} chars: {line}", line.len());
        }
        assert!(lines[1].starts_with("    "), "continuation is indented");
        assert!(lines[1].contains("INPUT"), "{line}");
    }

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

    #[test]
    fn protocol_hashes_roundtrip_as_compact_named_ron() {
        let hashes = ProtocolHashes {
            messages: Some(1),
            components: Some(2),
            channels: Some(3),
        };
        let text = ron_compact(&hashes);
        assert!(text.starts_with("ProtocolHashes("), "{text}");
        assert!(text.contains("components: Some(2)"), "{text}");
        assert!(!text.contains('\n'), "{text}");

        let decoded: ProtocolHashes = ron::from_str(&text).expect("RON roundtrip");
        assert_eq!(decoded, hashes);
    }

    #[test]
    fn pretty_ron_is_multiline_and_named() {
        let text = ron_pretty(&ProtocolHashes {
            messages: None,
            components: None,
            channels: None,
        });
        assert!(text.contains("ProtocolHashes("), "{text}");
        assert!(text.lines().count() > 3, "{text}");
    }

    #[test]
    fn one_line_output_summarizes_an_input_message() {
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

        // An idle input message captured from the client: net id 4, no
        // interpolation delay, `end_tick` 177, no targets, not a rebroadcast.
        let payload = [0x04u8, 0x00, 0xb1, 0x01, 0x00, 0x00];

        let mut bytes = vec![0u8]; // Data packet
        bytes.extend_from_slice(&9u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&5u32.to_be_bytes());
        put_varint(&mut bytes, 5); // InputChannel
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
        assert!(line.contains("INPUT"), "{line}");
        assert!(line.contains("net_id=4"), "{line}");
        assert!(line.contains("end_tick=177"), "{line}");
        assert!(line.contains("targets=0"), "{line}");
        assert!(line.len() <= 100, "{} chars: {line}", line.len());
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
        assert!(line.len() <= 100, "{} chars: {line}", line.len());
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
        assert!(!packet_allows_filter(&hide_ping, &parsed));
        let show_ping = filter(&["ping"]);
        assert!(packet_allows_filter(&show_ping, &parsed));

        // A mixed datagram stays visible while any message matches.
        let mixed = data_datagram(&[&[0x00, 0x12, 0x34], &[4, 0xff, 0x00]], 0);
        let mixed = Packet::parse(&mixed);
        assert!(packet_allows_filter(&filter(&["!ping"]), &mixed));
        assert!(!packet_allows_filter(
            &filter(&["!ping", "!internal"]),
            &mixed
        ));

        // An ACK carries no messages and only matches the `ack` name.
        let mut ack = vec![4u8];
        ack.extend_from_slice(&1u16.to_be_bytes());
        ack.extend_from_slice(&0u16.to_be_bytes());
        ack.extend_from_slice(&0u32.to_be_bytes());
        ack.extend_from_slice(&7u32.to_be_bytes());
        let ack = Packet::parse(&ack);
        assert!(packet_allows_filter(&hide_ping, &ack));
        assert!(!packet_allows_filter(&show_ping, &ack));
    }
}
