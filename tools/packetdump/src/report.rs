//! Packet rendering and aggregate statistics.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use proto::{CollisionCorrection, PlayerMovementPacket, ServerSnapshot, peer_mode};

use crate::message::{self, Decoded, Direction};
use crate::relay::Datagram;
use crate::wire::{Batch, Body, Fragment, Packet, WireError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectionFilter {
    Both,
    ClientToServer,
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

        let visible =
            self.opts.directions.shows(datagram.direction) && (self.opts.show_acks || !ack_only);
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
                    match message::decode(*channel, direction, fragment.payload) {
                        Decoded::Input { packet, .. } => {
                            out.push_str("  ");
                            out.push_str(&self.input_summary(&packet));
                        }
                        Decoded::Snapshot { packet, .. } => {
                            out.push_str("  ");
                            out.push_str(&self.snapshot_summary(&packet));
                        }
                        Decoded::Ping { id, .. } => out.push_str(&format!("  ping id={id}")),
                        Decoded::Pong {
                            id,
                            tick_diff,
                            overstep,
                            ..
                        } => out.push_str(&format!(
                            "  pong id={id} frame_ticks={tick_diff} overstep={overstep}"
                        )),
                        Decoded::Unknown { .. } => {}
                    }
                }
                out
            }
            Body::Data(batches) => {
                let mut parts = Vec::new();
                for batch in batches {
                    for message in &batch.messages {
                        parts.push(self.message_summary(batch.channel, direction, message.payload));
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

    fn message_summary(&self, channel: u16, direction: Direction, payload: &[u8]) -> String {
        match message::decode(channel, direction, payload) {
            Decoded::Input { packet, .. } => self.input_summary(&packet),
            Decoded::Snapshot { packet, .. } => self.snapshot_summary(&packet),
            Decoded::Ping { net_id, id } => {
                format!("{}id={id} net_id={net_id}", self.kind("33", "PING"))
            }
            Decoded::Pong {
                net_id,
                id,
                tick_diff,
                overstep,
            } => format!(
                "{}id={id} frame_ticks={tick_diff} overstep={overstep} net_id={net_id}",
                self.kind("33", "PONG")
            ),
            Decoded::Unknown { net_id, bytes } => format!(
                "{}net_id={} {} B (not decoded)",
                self.kind("90", "INTERNAL"),
                net_id.map_or_else(|| "?".to_string(), |id| id.to_string()),
                bytes
            ),
        }
    }

    fn input_summary(&self, packet: &PlayerMovementPacket) -> String {
        format!(
            "{}seq={} player={} t={} pos={} vel={} yaw={} grounded={} applied={} inputs={}",
            self.kind("36", "INPUT"),
            packet.seq,
            packet.player,
            packet.report.tick.0,
            fmt_vec(packet.report.position),
            fmt_vec(packet.report.velocity),
            fmt_yaw(packet.report.body_yaw),
            packet.report.grounded,
            packet.report.applied_correction_seq,
            packet.inputs.len()
        )
    }

    fn snapshot_summary(&self, packet: &ServerSnapshot) -> String {
        format!(
            "{}t={} ack={} shadow={} {} peers={} resync={}",
            self.kind("32", "SNAPSHOT"),
            packet.server_tick.0,
            packet.acked_seq,
            fmt_vec(packet.shadow_position),
            correction_summary(&packet.correction),
            packet.peers.len(),
            packet.resync
        )
    }

    fn print_batch(&self, batch: &Batch<'_>, direction: Direction) {
        let name = channel_name(batch, direction);
        println!("  channel {}{}", batch.channel, name);
        for (index, message) in batch.messages.iter().enumerate() {
            let message_id = message
                .id
                .map_or_else(String::new, |id| format!("  msg_id={id}"));
            match message::decode(batch.channel, direction, message.payload) {
                Decoded::Input { net_id, packet } => {
                    println!(
                        "    msg {index}  PlayerMovementPacket  net_id={net_id}  {} B{message_id}",
                        message.payload.len()
                    );
                    self.print_input(&packet);
                }
                Decoded::Snapshot { net_id, packet } => {
                    println!(
                        "    msg {index}  ServerSnapshot  net_id={net_id}  {} B{message_id}",
                        message.payload.len()
                    );
                    self.print_snapshot(&packet);
                }
                Decoded::Ping { net_id, id } => {
                    println!(
                        "    msg {index}  Ping  net_id={net_id}  {} B{message_id}",
                        message.payload.len()
                    );
                    println!("      id={id}");
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
                    println!(
                        "      ping_id={id}  frame_time_ticks={tick_diff}  overstep={overstep}"
                    );
                }
                Decoded::Unknown { net_id, bytes } => println!(
                    "    msg {index}  internal  net_id={net_id:?}  {bytes} B (not decoded)",
                ),
            }
        }
    }

    fn print_input(&self, packet: &PlayerMovementPacket) {
        println!("      seq={}  player={}", packet.seq, packet.player);
        println!("      report");
        println!(
            "        tick={}  pos={}  vel={}",
            packet.report.tick.0,
            fmt_vec(packet.report.position),
            fmt_vec(packet.report.velocity)
        );
        println!(
            "        body_yaw={}  grounded={}  applied_correction={}",
            fmt_yaw(packet.report.body_yaw),
            packet.report.grounded,
            packet.report.applied_correction_seq
        );
        if packet.inputs.is_empty() {
            println!("      inputs: none");
        } else {
            println!("      inputs ({}, newest first)", packet.inputs.len());
            for (index, input) in packet.inputs.iter().enumerate() {
                println!(
                    "        [{index}] tick={}  move=({}, {})  buttons={}  camera_yaw={}",
                    input.tick.0,
                    input.move_x,
                    input.move_z,
                    fmt_buttons(input.buttons),
                    fmt_yaw(input.camera_yaw_rad()),
                );
            }
        }
    }

    fn print_snapshot(&self, packet: &ServerSnapshot) {
        println!(
            "      server_tick={}  acked_seq={}  resync={}",
            packet.server_tick.0, packet.acked_seq, packet.resync
        );
        println!("      shadow={}", fmt_vec(packet.shadow_position));
        match &packet.correction {
            None => println!("      correction: none"),
            Some(correction) => {
                println!(
                    "      correction seq={}  tick={}  epoch={}  dv={}",
                    correction.seq,
                    correction.tick.0,
                    correction.epoch,
                    fmt_vec(correction.velocity_delta)
                );
                match correction.hard_normal {
                    Some(normal) => println!("        hard_normal={}", fmt_vec(normal)),
                    None => println!("        hard_normal: none"),
                }
            }
        }
        if packet.peers.is_empty() {
            println!("      peers: none");
        } else {
            println!("      peers ({})", packet.peers.len());
            for (index, peer) in packet.peers.iter().enumerate() {
                println!(
                    "        [{index}] player={}  pos={}  yaw={}  mode={}",
                    peer.player,
                    fmt_vec(peer.position),
                    fmt_yaw(peer.body_yaw),
                    mode_name(peer.mode)
                );
            }
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
            match message::decode(channel, direction, fragment.payload) {
                Decoded::Input { packet, .. } => self.print_input(&packet),
                Decoded::Snapshot { packet, .. } => self.print_snapshot(&packet),
                Decoded::Ping { id, .. } => println!("    ping id={id}"),
                Decoded::Pong {
                    id,
                    tick_diff,
                    overstep,
                    ..
                } => println!(
                    "    pong ping_id={id} frame_time_ticks={tick_diff} overstep={overstep}"
                ),
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

fn record_messages(stats: &mut DirStats, packet: &Packet<'_>, direction: Direction) {
    fn add(stats: &mut DirStats, channel: u16, direction: Direction, payload: &[u8]) {
        let name = match message::decode(channel, direction, payload) {
            Decoded::Input { .. } => "INPUT",
            Decoded::Snapshot { .. } => "SNAPSHOT",
            Decoded::Ping { .. } => "PING",
            Decoded::Pong { .. } => "PONG",
            Decoded::Unknown { .. } => "INTERNAL",
        };
        let stat = stats.messages.entry(name).or_default();
        stat.count += 1;
        stat.bytes += payload.len() as u64;
    }

    match &packet.body {
        Body::Data(batches) => {
            for batch in batches {
                for message in &batch.messages {
                    add(stats, batch.channel, direction, message.payload);
                }
            }
        }
        Body::Fragment {
            channel,
            fragment,
            tail,
        } => {
            if fragment.index == 0 {
                add(stats, *channel, direction, fragment.payload);
            }
            for batch in tail {
                for message in &batch.messages {
                    add(stats, batch.channel, direction, message.payload);
                }
            }
        }
        Body::AckOnly | Body::Compressed => {}
    }
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

fn correction_summary(correction: &Option<CollisionCorrection>) -> String {
    match correction {
        None => "corr=none".to_string(),
        Some(correction) => {
            let mut out = format!(
                "corr=seq={} tick={} epoch={} dv={}",
                correction.seq,
                correction.tick.0,
                correction.epoch,
                fmt_vec(correction.velocity_delta)
            );
            if let Some(normal) = correction.hard_normal {
                out.push_str(&format!(" n={}", fmt_vec(normal)));
            }
            out
        }
    }
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

fn mode_name(mode: u8) -> &'static str {
    match mode {
        peer_mode::NONE => "none",
        peer_mode::SOFT => "soft",
        peer_mode::HARD => "hard",
        _ => "unknown",
    }
}

fn fmt_vec(v: [f32; 3]) -> String {
    format!("({:.3}, {:.3}, {:.3})", v[0], v[1], v[2])
}

fn fmt_yaw(rad: f32) -> String {
    format!("{:.1}deg", rad.to_degrees())
}

fn fmt_buttons(buttons: u8) -> String {
    let mut names = Vec::new();
    if buttons & proto::buttons::JUMP != 0 {
        names.push("JUMP".to_string());
    }
    if buttons & proto::buttons::SPRINT != 0 {
        names.push("SPRINT".to_string());
    }
    if buttons & proto::buttons::CROUCH != 0 {
        names.push("CROUCH".to_string());
    }
    let unknown =
        buttons & !(proto::buttons::JUMP | proto::buttons::SPRINT | proto::buttons::CROUCH);
    if unknown != 0 {
        names.push(format!("0x{unknown:02x}"));
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("|")
    }
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

    #[test]
    fn correction_summary_includes_velocity_and_hard_normal() {
        let correction = Some(CollisionCorrection {
            seq: 7,
            tick: proto::Tick::new(90),
            epoch: 2,
            velocity_delta: [1.0, 0.0, -2.0],
            hard_normal: Some([0.0, 1.0, 0.0]),
        });
        let text = correction_summary(&correction);
        assert!(text.contains("seq=7"), "{text}");
        assert!(text.contains("epoch=2"), "{text}");
        assert!(text.contains("(1.000, 0.000, -2.000)"), "{text}");
        assert!(text.contains("n=(0.000, 1.000, 0.000)"), "{text}");
        assert_eq!(correction_summary(&None), "corr=none");
    }

    #[test]
    fn input_buttons_are_named() {
        assert_eq!(fmt_buttons(0), "none");
        assert_eq!(fmt_buttons(proto::buttons::JUMP), "JUMP");
        assert_eq!(
            fmt_buttons(proto::buttons::JUMP | proto::buttons::SPRINT),
            "JUMP|SPRINT"
        );
        assert_eq!(fmt_buttons(0x80), "0x80");
        assert_eq!(fmt_buttons(proto::buttons::CROUCH | 0x80), "CROUCH|0x80");
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
        });

        let line = reporter.format_line(7, Duration::from_millis(1234), &datagram, &parsed);
        assert!(line.contains("S->C"), "{line}");
        assert!(line.contains(":54321"), "{line}");
        assert!(line.contains("pkt=9"), "{line}");
        assert!(line.contains("ACK"), "{line}");
    }
}
