//! On-disk configuration: which packets the analyzer prints.
//!
//! The file is `packetdump.toml` in the working directory, or the copy shipped
//! in this crate when it is run through cargo. Every `filter` entry is a packet
//! name or a lightyear message net id; prefixing it with `!` hides that packet
//! instead of selecting it. Exclusions win over inclusions, and a datagram is
//! printed when at least one of its messages matches.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::message::{Kind, MessageKey};

/// Config file name looked up when `--config` is not given.
pub const FILE_NAME: &str = "packetdump.toml";

const KNOWN_NAMES: &str = "input, snapshot, ping, pong, internal or ack";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Packet names or net ids to print; prefix with `!` to hide.
    #[serde(default)]
    filter: Vec<String>,
}

/// The parsed config file plus the path it came from, if any file was used.
#[derive(Debug)]
pub struct Config {
    pub filter: PacketFilter,
    pub path: Option<PathBuf>,
}

/// Loads `explicit`, or the default file when it exists. A missing default is
/// not an error; an unreadable or invalid file is.
pub fn load(explicit: Option<&Path>) -> Result<Config, String> {
    let path = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => find_default(),
    };
    let Some(path) = path else {
        return Ok(Config {
            filter: PacketFilter::default(),
            path: None,
        });
    };

    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let file: FileConfig =
        toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let filter = PacketFilter::parse(&file.filter)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Config {
        filter,
        path: Some(path),
    })
}

fn find_default() -> Option<PathBuf> {
    let local = PathBuf::from(FILE_NAME);
    if local.is_file() {
        return Some(local);
    }
    // `cargo run -p packetdump` normally starts in the repository root, so also
    // honour the file that ships with the tool.
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join(FILE_NAME);
    shipped.is_file().then_some(shipped)
}

/// A set of include/exclude rules applied to the packet log.
#[derive(Clone, Debug, Default)]
pub struct PacketFilter {
    rules: Vec<Rule>,
}

#[derive(Clone, Copy, Debug)]
struct Rule {
    exclude: bool,
    target: Target,
}

#[derive(Clone, Copy, Debug)]
enum Target {
    Kind(Kind),
    NetId(u16),
}

impl PacketFilter {
    pub fn parse(entries: &[String]) -> Result<Self, String> {
        let rules = entries
            .iter()
            .map(|entry| Rule::parse(entry))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { rules })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Returns true when `key` survives the filter.
    pub fn allows(&self, key: MessageKey) -> bool {
        if self
            .rules
            .iter()
            .any(|rule| rule.exclude && rule.matches(key))
        {
            return false;
        }
        let mut includes = self.rules.iter().filter(|rule| !rule.exclude).peekable();
        includes.peek().is_none() || includes.any(|rule| rule.matches(key))
    }
}

impl Rule {
    fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        let (exclude, name) = match trimmed.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, trimmed),
        };
        if name.is_empty() {
            return Err(format!("empty packet filter entry {raw:?}"));
        }
        let target = match name.parse::<u16>() {
            Ok(id) => Target::NetId(id),
            Err(_) => Target::Kind(kind_from_name(name).ok_or_else(|| {
                format!("unknown packet name {name:?}; use {KNOWN_NAMES} or a net id")
            })?),
        };
        Ok(Self { exclude, target })
    }

    fn matches(self, key: MessageKey) -> bool {
        match self.target {
            Target::Kind(kind) => key.kind == kind,
            Target::NetId(id) => key.net_id == Some(id),
        }
    }
}

impl Target {
    fn label(self) -> String {
        match self {
            Self::Kind(kind) => kind.name().to_ascii_lowercase(),
            Self::NetId(id) => format!("net_id={id}"),
        }
    }
}

/// Names are case-insensitive and ignore `_` and `-`, so `PlayerMovementPacket`
/// and `player-movement-packet` both work.
fn kind_from_name(name: &str) -> Option<Kind> {
    let normalized: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    match normalized.as_str() {
        "input" | "playermovementpacket" => Some(Kind::Input),
        "snapshot" | "serversnapshot" => Some(Kind::Snapshot),
        "ping" => Some(Kind::Ping),
        "pong" => Some(Kind::Pong),
        "internal" | "unknown" | "compressed" => Some(Kind::Internal),
        "ack" | "ackonly" => Some(Kind::Ack),
        _ => None,
    }
}

impl fmt::Display for PacketFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.rules.is_empty() {
            return f.write_str("all packets");
        }
        let mut written = false;
        for (exclude, verb) in [(false, "show"), (true, "hide")] {
            let labels: Vec<String> = self
                .rules
                .iter()
                .filter(|rule| rule.exclude == exclude)
                .map(|rule| rule.target.label())
                .collect();
            if labels.is_empty() {
                continue;
            }
            if written {
                f.write_str("; ")?;
            }
            written = true;
            write!(f, "{verb} {}", labels.join(", "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(entries: &[&str]) -> PacketFilter {
        let entries: Vec<String> = entries.iter().map(|entry| entry.to_string()).collect();
        PacketFilter::parse(&entries).expect("valid filter")
    }

    fn key(kind: Kind) -> MessageKey {
        MessageKey { kind, net_id: None }
    }

    #[test]
    fn an_empty_filter_allows_every_packet() {
        let filter = PacketFilter::default();
        for kind in [
            Kind::Input,
            Kind::Snapshot,
            Kind::Ping,
            Kind::Pong,
            Kind::Internal,
            Kind::Ack,
        ] {
            assert!(filter.allows(key(kind)), "{} was hidden", kind.name());
        }
    }

    #[test]
    fn a_bare_entry_selects_only_that_packet() {
        let filter = filter(&["input"]);
        assert!(filter.allows(key(Kind::Input)));
        assert!(!filter.allows(key(Kind::Snapshot)));
        assert!(!filter.allows(MessageKey::ack()), "ACK is a packet too");
    }

    #[test]
    fn a_bang_entry_hides_that_packet() {
        let filter = filter(&["!ping", "!pong"]);
        assert!(!filter.allows(key(Kind::Ping)));
        assert!(!filter.allows(key(Kind::Pong)));
        assert!(filter.allows(key(Kind::Input)));
        assert!(filter.allows(MessageKey::ack()));
    }

    #[test]
    fn exclusion_wins_over_inclusion() {
        let filter = filter(&["input", "!input"]);
        assert!(!filter.allows(key(Kind::Input)));
        assert!(!filter.allows(key(Kind::Snapshot)));
    }

    #[test]
    fn names_are_case_insensitive_and_match_the_struct_names() {
        let filter = filter(&["PlayerMovementPacket", "server_snapshot"]);
        assert!(filter.allows(key(Kind::Input)));
        assert!(filter.allows(key(Kind::Snapshot)));
        assert!(!filter.allows(key(Kind::Ping)));
    }

    #[test]
    fn a_number_matches_the_message_net_id() {
        let filter = filter(&["!3"]);
        assert!(!filter.allows(MessageKey {
            kind: Kind::Snapshot,
            net_id: Some(3),
        }));
        assert!(filter.allows(MessageKey {
            kind: Kind::Snapshot,
            net_id: Some(4),
        }));
        assert!(filter.allows(key(Kind::Internal)), "no net id to match");
    }

    #[test]
    fn malformed_entries_are_rejected() {
        let parse_one = |entry: &str| PacketFilter::parse(&[entry.to_string()]);
        assert!(parse_one("!").is_err(), "bare exclusion");
        assert!(parse_one("nonsense").is_err(), "unknown name");
        assert!(parse_one("70000").is_err(), "id outside u16");
    }

    #[test]
    fn the_summary_names_both_sides() {
        assert_eq!(
            filter(&["input", "!ping"]).to_string(),
            "show input; hide ping"
        );
        assert_eq!(filter(&["!0", "!1"]).to_string(), "hide net_id=0, net_id=1");
        assert_eq!(PacketFilter::default().to_string(), "all packets");
    }

    #[test]
    fn the_file_format_reads_the_filter_array() {
        let file: FileConfig = toml::from_str("filter = [\"input\", \"!5\"]").expect("config");
        assert_eq!(file.filter, ["input", "!5"]);
        assert!(
            toml::from_str::<FileConfig>("filterr = []").is_err(),
            "unknown keys must not be ignored"
        );
    }

    #[test]
    fn an_explicit_config_file_is_loaded() {
        let path =
            std::env::temp_dir().join(format!("packetdump-{}-explicit.toml", std::process::id()));
        fs::write(&path, "filter = [\"!ping\"]\n").expect("write config");
        let loaded = load(Some(&path));
        fs::remove_file(&path).ok();

        let loaded = loaded.expect("load config");
        assert_eq!(loaded.path.as_deref(), Some(path.as_path()));
        assert!(!loaded.filter.allows(key(Kind::Ping)));
        assert!(loaded.filter.allows(MessageKey {
            kind: Kind::Input,
            net_id: Some(2),
        }));
    }

    #[test]
    fn a_missing_explicit_config_is_an_error() {
        let error = load(Some(Path::new("missing-packetdump.toml"))).expect_err("must fail");
        assert!(error.contains("missing-packetdump.toml"), "{error}");
    }
}
