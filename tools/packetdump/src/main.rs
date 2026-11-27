//! Developer-only packet analyzer for the lightyear protocol.
//!
//! This is a transparent UDP relay, not a pcap tap: the client points at the
//! relay and the relay forwards every datagram to the real server while
//! decoding lightyear framing, the input and protocol-metadata messages,
//! replicon traffic and ping/pong. That makes it usable on loopback without
//! Npcap or administrator rights, and the server needs no configuration change.

mod cli;
mod config;
mod message;
mod relay;
mod report;
mod wire;

use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;

use report::{Options, Reporter};

const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    let args = cli::Args::parse();
    let listen = args.listen;
    let server = args.server;

    let config = match config::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("packetdump: {error}");
            return ExitCode::from(2);
        }
    };

    let mut relay = match relay::Relay::bind(listen, server) {
        Ok(relay) => relay,
        Err(error) => {
            eprintln!("packetdump: cannot bind {listen}: {error}");
            return ExitCode::from(2);
        }
    };
    let bound = relay.local_addr().unwrap_or(listen);
    let connect = match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::from(([127, 0, 0, 1], bound.port())),
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], bound.port()))
        }
        _ => bound,
    };
    let color =
        !args.no_color && std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();

    println!("packetdump  parsed Lightyear packets (dev tool)");
    println!("  relay   {bound} -> {server}");
    println!("  client  --connect {connect}");
    if !config.filter.is_empty() {
        match &config.path {
            Some(path) => println!("  filter  {}  ({})", config.filter, path.display()),
            None => println!("  filter  {}", config.filter),
        }
    }
    println!();

    let mut reporter = Reporter::new(Options {
        detail: args.detail,
        hex: args.hex,
        show_acks: !args.no_acks,
        directions: args.directions,
        color,
        stats_every: (args.stats_every > 0.0).then(|| Duration::from_secs_f32(args.stats_every)),
        limit: args.count,
        filter: config.filter,
    });

    loop {
        if let Err(error) = relay.poll(POLL_INTERVAL, &mut |datagram| {
            reporter.on_datagram(datagram);
        }) {
            eprintln!("packetdump: relay error: {error}");
        }
        reporter.tick(Instant::now());
        if reporter.limit_reached() {
            break;
        }
    }

    reporter.finish();
    ExitCode::SUCCESS
}
