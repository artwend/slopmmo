//! Command line for the packet analyzer.

use std::net::SocketAddr;

use crate::report::DirectionFilter;

#[derive(Clone, Debug)]
pub struct Args {
    pub listen: String,
    pub server: String,
    pub count: u64,
    pub stats_every: f32,
    pub detail: bool,
    pub hex: bool,
    pub no_acks: bool,
    pub directions: DirectionFilter,
    pub color: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:4001".to_string(),
            server: "127.0.0.1:4000".to_string(),
            count: 0,
            stats_every: 1.0,
            detail: false,
            hex: false,
            no_acks: false,
            directions: DirectionFilter::Both,
            color: true,
        }
    }
}

impl Args {
    pub fn parse() -> Self {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from<I>(args: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let mut out = Self::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-l" | "--listen" => out.listen = value(&mut it, "--listen"),
                "-s" | "--server" => out.server = value(&mut it, "--server"),
                "-n" | "--count" => out.count = number(&mut it, "--count"),
                "-t" | "--stats-every" => {
                    out.stats_every = decimal(&mut it, "--stats-every");
                    if !out.stats_every.is_finite()
                        || out.stats_every < 0.0
                        || out.stats_every > 86_400.0
                    {
                        fail("--stats-every must be between 0 and 86400 seconds");
                    }
                }
                "-v" | "--detail" => out.detail = true,
                "-x" | "--hex" => out.hex = true,
                "-a" | "--no-acks" => out.no_acks = true,
                "-d" | "--dir" => {
                    out.directions = match value(&mut it, "--dir").as_str() {
                        "c2s" => DirectionFilter::ClientToServer,
                        "s2c" => DirectionFilter::ServerToClient,
                        "both" => DirectionFilter::Both,
                        other => fail(&format!("--dir must be c2s, s2c or both, got {other:?}")),
                    }
                }
                "--no-color" => out.color = false,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => fail(&format!("unknown argument: {other}")),
            }
        }
        out
    }

    pub fn listen_addr(&self) -> SocketAddr {
        address(&self.listen, "--listen")
    }

    pub fn server_addr(&self) -> SocketAddr {
        address(&self.server, "--server")
    }
}

fn value(it: &mut impl Iterator<Item = String>, flag: &str) -> String {
    it.next()
        .unwrap_or_else(|| fail(&format!("{flag} needs a value")))
}

fn number(it: &mut impl Iterator<Item = String>, flag: &str) -> u64 {
    let raw = value(it, flag);
    raw.parse()
        .unwrap_or_else(|_| fail(&format!("{flag} needs a number, got {raw:?}")))
}

fn decimal(it: &mut impl Iterator<Item = String>, flag: &str) -> f32 {
    let raw = value(it, flag);
    raw.parse()
        .unwrap_or_else(|_| fail(&format!("{flag} needs a number, got {raw:?}")))
}

fn address(raw: &str, flag: &str) -> SocketAddr {
    raw.parse()
        .unwrap_or_else(|e| fail(&format!("invalid {flag} address {raw:?}: {e}")))
}

fn fail(message: &str) -> ! {
    eprintln!("packetdump: {message}");
    eprintln!("try `packetdump --help`");
    std::process::exit(2);
}

fn print_help() {
    println!(
        "usage: packetdump [--listen ADDR] [--server ADDR] [options]\n\
         \n\
         Relays UDP traffic through this process and prints every Lightyear packet with\n\
         its decoded messages as RON. Point the client at the relay instead of the server:\n\
         \n\
         \x20 cargo run -p server -- --listen 127.0.0.1:4000\n\
         \x20 cargo run -p packetdump -- --listen 127.0.0.1:4001 --server 127.0.0.1:4000\n\
         \x20 cargo run -p client -- --headless --connect 127.0.0.1:4001\n\
         \n\
         options:\n\
         \x20 -l, --listen ADDR        relay bind address (default 127.0.0.1:4001)\n\
         \x20 -s, --server ADDR        server address to forward to (default 127.0.0.1:4000)\n\
         \x20 -n, --count N            stop after N observed datagrams (default 0: until Ctrl+C)\n\
         \x20 -t, --stats-every SECS   print rate stats every SECS, 0 disables (default 1.0)\n\
         \x20 -v, --detail             pretty-print the decoded RON payload\n\
         \x20 -x, --hex                append a hex dump of each datagram\n\
         \x20 -a, --no-acks            hide header-only ACK packets\n\
         \x20 -d, --dir MODE           c2s, s2c or both (default both)\n\
         \x20     --no-color          disable ANSI colors\n\
         \x20 -h, --help               show this help\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(parts: &[&str]) -> Args {
        Args::parse_from(parts.iter().map(|part| (*part).to_string()))
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
        assert!(!long.color);
    }
}
