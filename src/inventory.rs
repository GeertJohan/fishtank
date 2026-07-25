//! Machine inventory: the list of BMCs fishtank polls.
//!
//! The inventory is a plain **JSON5** document (JSON is a subset, so plain JSON
//! works too). fishtank does no decryption or secret management of its own —
//! that is deliberately left to whatever produces the inventory. To consume
//! secrets held elsewhere, generate the inventory on the fly and pipe it in:
//!
//! ```text
//! my-secret-tool | fishtank --machines /dev/stdin
//! ```
//!
//! `--machines` accepts any path; `-` and `/dev/stdin` read from a pipe.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result, eyre};
use serde::Deserialize;

use crate::config::get_config_dir;

/// Default power/boot-state poll interval if the inventory doesn't set one.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;

/// Default health (sensor) poll interval. Health changes slowly and the read is
/// expensive over IPMI, so it runs on a slower beat than the power poll.
const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 120;

/// Which protocol to use when talking to a machine's BMC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Ipmi,
    Redfish,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Protocol::Ipmi => f.write_str("ipmi"),
            Protocol::Redfish => f.write_str("redfish"),
        }
    }
}

/// A fully-resolved machine, ready to poll (defaults already folded in).
#[derive(Debug, Clone)]
pub struct Machine {
    pub name: String,
    pub protocol: Protocol,
    /// BMC IP or hostname.
    pub host: String,
    /// BMC MAC address, if known from the inventory.
    pub mac: Option<String>,
    /// Serial known from the inventory; otherwise discovered by polling.
    pub serial: Option<String>,
    pub username: String,
    pub password: String,
    // Redfish-only knobs:
    pub scheme: String,
    pub port: Option<u16>,
    pub insecure: bool,
}

/// The resolved inventory handed to the app.
#[derive(Debug, Clone)]
pub struct Inventory {
    pub machines: Vec<Machine>,
    /// How often to poll power/boot-state.
    pub poll_interval_secs: u64,
    /// How often to poll health (sensors) — slower; the read is expensive.
    pub health_interval_secs: u64,
}

// --- Raw (on-the-wire) shapes ----------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    machines: Vec<RawMachine>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    username: Option<String>,
    password: Option<String>,
    poll_interval_secs: Option<u64>,
    health_interval_secs: Option<u64>,
    insecure: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMachine {
    name: String,
    protocol: Protocol,
    host: String,
    mac: Option<String>,
    serial: Option<String>,
    username: Option<String>,
    password: Option<String>,
    scheme: Option<String>,
    port: Option<u16>,
    insecure: Option<bool>,
}

impl Inventory {
    fn from_raw(raw: RawConfig) -> Self {
        let d = &raw.defaults;
        let machines = raw
            .machines
            .into_iter()
            .map(|m| Machine {
                username: m
                    .username
                    .or_else(|| d.username.clone())
                    .unwrap_or_else(|| "ADMIN".to_string()),
                password: m
                    .password
                    .or_else(|| d.password.clone())
                    .unwrap_or_default(),
                scheme: m.scheme.unwrap_or_else(|| "https".to_string()),
                insecure: m.insecure.or(d.insecure).unwrap_or(false),
                name: m.name,
                protocol: m.protocol,
                host: m.host,
                mac: m.mac,
                serial: m.serial,
                port: m.port,
            })
            .collect();

        Inventory {
            machines,
            poll_interval_secs: d.poll_interval_secs.unwrap_or(DEFAULT_POLL_INTERVAL_SECS),
            health_interval_secs: d
                .health_interval_secs
                .unwrap_or(DEFAULT_HEALTH_INTERVAL_SECS),
        }
    }

    /// A built-in fake inventory for `--demo` / the agent harness.
    pub fn demo() -> Self {
        let make = |name: &str, protocol: Protocol, host: &str, mac: &str| Machine {
            name: name.to_string(),
            protocol,
            host: host.to_string(),
            mac: Some(mac.to_string()),
            serial: None,
            username: "ADMIN".to_string(),
            password: String::new(),
            scheme: "https".to_string(),
            port: None,
            insecure: true,
        };
        Inventory {
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            health_interval_secs: DEFAULT_HEALTH_INTERVAL_SECS,
            machines: vec![
                make(
                    "dc1-r01-node01",
                    Protocol::Ipmi,
                    "10.130.128.10",
                    "90:5a:08:17:ae:01",
                ),
                make(
                    "dc1-r01-node02",
                    Protocol::Ipmi,
                    "10.130.128.11",
                    "90:5a:08:17:ae:02",
                ),
                make(
                    "dc1-r01-node03",
                    Protocol::Ipmi,
                    "10.130.128.12",
                    "90:5a:08:17:ae:03",
                ),
                make(
                    "dc1-r02-node01",
                    Protocol::Redfish,
                    "10.130.129.10",
                    "90:5a:08:17:bf:01",
                ),
                make(
                    "dc1-r02-node02",
                    Protocol::Redfish,
                    "10.130.129.11",
                    "90:5a:08:17:bf:02",
                ),
                make(
                    "dc2-r01-node01",
                    Protocol::Ipmi,
                    "10.131.128.10",
                    "90:5a:08:18:ae:01",
                ),
                make(
                    "dc2-r01-node02",
                    Protocol::Ipmi,
                    "10.131.128.13",
                    "90:5a:08:18:ae:02",
                ),
                make(
                    "dc2-r01-node03",
                    Protocol::Ipmi,
                    "10.131.128.14",
                    "90:5a:08:18:ae:03",
                ),
            ],
        }
    }
}

/// Load and resolve the inventory from `path`, or by discovery if `None`.
pub fn load(path: Option<&Path>) -> Result<Inventory> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => discover().ok_or_else(|| {
            eyre!(
                "no inventory found; pass --machines <path> (use - or /dev/stdin to \
                 read a pipe) or create fishtank-machines.json5 (or .json) in the \
                 current or config directory"
            )
        })?,
    };

    let text = read_text(&path).with_context(|| format!("reading inventory {}", path.display()))?;
    let raw: RawConfig = json5::from_str(&text)
        .with_context(|| format!("parsing inventory {} (expected JSON5)", path.display()))?;
    Ok(Inventory::from_raw(raw))
}

/// Read the inventory text. `-` and `/dev/stdin` read from standard input.
fn read_text(path: &Path) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        Ok(std::fs::read_to_string(path)?)
    }
}

/// Look for a default inventory in CWD then the config directory.
fn discover() -> Option<PathBuf> {
    let names = ["fishtank-machines.json5", "fishtank-machines.json"];
    let dirs = [PathBuf::from("."), get_config_dir()];
    for dir in &dirs {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
