//! Command line for the packet analyzer.

use std::net::SocketAddr;

use clap::Parser;

use crate::report::DirectionFilter;

/// Relay UDP traffic through this process and print every Lightyear packet with
/// its decoded messages as RON.
///
/// Point the client at the relay instead of the server:
///
///   cargo run -p server -- --listen 127.0.0.1:4000
///   cargo run -p packetdump -- --listen 127.0.0.1:4001 --server 127.0.0.1:4000
///   cargo run -p client -- --headless --connect 127.0.0.1:4001
#[derive(Clone, Debug, Parser)]
#[command(name = "packetdump", verbatim_doc_comment)]
pub struct Args {
    /// relay bind address
    #[arg(
        short = 'l',
        long,
        value_name = "ADDR",
        default_value = "127.0.0.1:4001"
    )]
    pub listen: SocketAddr,
    /// server address to forward to
    #[arg(
        short = 's',
        long,
        value_name = "ADDR",
        default_value = "127.0.0.1:4000"
    )]
    pub server: SocketAddr,
    /// stop after N observed datagrams (0: until Ctrl+C)
    #[arg(short = 'n', long, value_name = "N", default_value_t = 0)]
    pub count: u64,
    /// print rate stats every SECS, 0 disables
    #[arg(
        short = 't',
        long,
        value_name = "SECS",
        default_value_t = 1.0,
        value_parser = parse_stats_every
    )]
    pub stats_every: f32,
    /// pretty-print the decoded RON payload
    #[arg(short = 'v', long)]
    pub detail: bool,
    /// append a hex dump of each datagram
    #[arg(short = 'x', long)]
    pub hex: bool,
    /// hide header-only ACK packets
    #[arg(short = 'a', long)]
    pub no_acks: bool,
    /// which direction to show
    #[arg(
        short = 'd',
        long = "dir",
        value_name = "MODE",
        value_enum,
        default_value = "both"
    )]
    pub directions: DirectionFilter,
    /// disable ANSI colors
    #[arg(long)]
    pub no_color: bool,
}

fn parse_stats_every(raw: &str) -> Result<f32, String> {
    let secs: f32 = raw
        .parse()
        .map_err(|_| format!("expected a number of seconds, got {raw:?}"))?;
    if !secs.is_finite() || !(0.0..=86_400.0).contains(&secs) {
        return Err("must be between 0 and 86400 seconds".to_string());
    }
    Ok(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(parts: &[&str]) -> Args {
        Args::parse_from(std::iter::once("packetdump").chain(parts.iter().copied()))
    }

    #[test]
    fn short_flags_match_the_long_forms() {
        let short = parse(&[
            "-l",
            "0.0.0.0:5000",
            "-s",
            "127.0.0.1:6000",
            "-n",
            "12",
            "-t",
            "0.5",
            "-v",
            "-x",
            "-a",
            "-d",
            "s2c",
        ]);
        let long = parse(&[
            "--listen",
            "0.0.0.0:5000",
            "--server",
            "127.0.0.1:6000",
            "--count",
            "12",
            "--stats-every",
            "0.5",
            "--detail",
            "--hex",
            "--no-acks",
            "--dir",
            "s2c",
            "--no-color",
        ]);

        assert_eq!(short.listen, long.listen);
        assert_eq!(short.server, long.server);
        assert_eq!(short.count, 12);
        assert_eq!(short.stats_every, 0.5);
        assert!(short.detail);
        assert!(short.hex);
        assert!(short.no_acks);
        assert_eq!(short.directions, DirectionFilter::ServerToClient);
        assert!(long.no_color);
    }

    #[test]
    fn out_of_range_stats_interval_is_rejected() {
        assert!(Args::try_parse_from(["packetdump", "-t", "90000"]).is_err());
        assert!(Args::try_parse_from(["packetdump", "-t", "slow"]).is_err());
        assert_eq!(parse(&["-t", "0"]).stats_every, 0.0);
    }
}
