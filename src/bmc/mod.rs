//! BMC backends and the shared poll types.
//!
//! Polling is split into two cadences:
//! - [`poll_power`] — the frequent, cheap liveness/boot-state probe.
//! - [`poll_health`] — the slower sensor read (expensive over IPMI).
//!
//! Which backend runs is decided by the machine's
//! [`Protocol`](crate::inventory::Protocol), or forced to the mock backend in
//! demo mode. Dispatch is a plain match over concrete backends (no
//! `dyn`/async-trait), matching ratata's static-composition preference.

pub mod ipmi;
pub mod mock;
pub mod redfish;

use std::time::{Duration, Instant};

use crate::inventory::{Machine, Protocol};

/// Overall per-poll budget. Exceeding it is treated as a failure (unreachable
/// for a power poll; no health update for a health poll). Backend calls are
/// cancelled (and their subprocesses killed) when this fires.
const POLL_TIMEOUT: Duration = Duration::from_secs(60);

/// Power state of the host behind a BMC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    On,
    Off,
    Unknown,
}

/// Aggregated health summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Warning,
    Critical,
    Unknown,
}

impl Health {
    /// Severity ordering for "take the worst sensor" aggregation.
    pub fn rank(self) -> u8 {
        match self {
            Health::Ok => 0,
            Health::Unknown => 1,
            Health::Warning => 2,
            Health::Critical => 3,
        }
    }

    /// The more severe of the two.
    pub fn worst(self, other: Health) -> Health {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// A power control action the operator can request on a machine. There are
/// several ways to "turn off" / restart: a hard cut vs. a graceful ACPI request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// Power on.
    On,
    /// Hard power off (pull the power, no OS involvement).
    ForceOff,
    /// Graceful ACPI shutdown request (asks the OS to power down).
    SoftOff,
    /// Power cycle (off, then on).
    Cycle,
    /// Hard reset (warm reset without a full power cut).
    Reset,
}

impl PowerAction {
    /// Human label for prompts and messages.
    pub fn label(self) -> &'static str {
        match self {
            PowerAction::On => "on",
            PowerAction::ForceOff => "force-off",
            PowerAction::SoftOff => "soft-off",
            PowerAction::Cycle => "cycle",
            PowerAction::Reset => "reset",
        }
    }

    /// `ipmitool chassis power <verb>`.
    pub fn ipmi_verb(self) -> &'static str {
        match self {
            PowerAction::On => "on",
            PowerAction::ForceOff => "off",
            PowerAction::SoftOff => "soft",
            PowerAction::Cycle => "cycle",
            PowerAction::Reset => "reset",
        }
    }

    /// Redfish `ComputerSystem.Reset` `ResetType`.
    pub fn redfish_reset_type(self) -> &'static str {
        match self {
            PowerAction::On => "On",
            PowerAction::ForceOff => "ForceOff",
            PowerAction::SoftOff => "GracefulShutdown",
            PowerAction::Cycle => "PowerCycle",
            PowerAction::Reset => "ForceRestart",
        }
    }

    /// The power state to optimistically show right after issuing the action
    /// (the next poll confirms). `None` for actions whose end state is the same
    /// or ambiguous (cycle/reset end up On but transiently off).
    pub fn optimistic_power(self) -> Option<PowerState> {
        match self {
            PowerAction::On => Some(PowerState::On),
            PowerAction::ForceOff | PowerAction::SoftOff => Some(PowerState::Off),
            PowerAction::Cycle | PowerAction::Reset => None,
        }
    }
}

/// A one-time boot-device override (e.g. "next boot from PXE"), as used during
/// provisioning. `None` means no override is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootOverride {
    None,
    Pxe,
    Disk,
    Bios,
    Cd,
    /// Some other override is set that we don't have a short name for.
    Other,
}

impl BootOverride {
    /// Short column label.
    pub fn short(self) -> &'static str {
        match self {
            BootOverride::None => "—",
            BootOverride::Pxe => "PXE",
            BootOverride::Disk => "disk",
            BootOverride::Bios => "bios",
            BootOverride::Cd => "cd",
            BootOverride::Other => "set",
        }
    }

    pub fn is_override(self) -> bool {
        !matches!(self, BootOverride::None)
    }
}

/// A next-boot override the operator can apply from the boot modal. Like power
/// actions, these are issued against a selection and confirmed with a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootAction {
    /// Boot from the network (PXE) on the next boot.
    Pxe,
    /// Boot from local disk on the next boot.
    Disk,
    /// Enter BIOS/firmware setup on the next boot.
    Bios,
    /// Boot from CD/DVD / virtual media on the next boot.
    Cd,
    /// Clear any override (boot normally).
    Clear,
}

impl BootAction {
    /// Human label for prompts and messages.
    pub fn label(self) -> &'static str {
        match self {
            BootAction::Pxe => "pxe",
            BootAction::Disk => "disk",
            BootAction::Bios => "bios",
            BootAction::Cd => "cd",
            BootAction::Clear => "clear",
        }
    }

    /// `ipmitool chassis bootdev <dev>` device name (`none` clears the override).
    pub fn ipmi_bootdev(self) -> &'static str {
        match self {
            BootAction::Pxe => "pxe",
            BootAction::Disk => "disk",
            BootAction::Bios => "bios",
            BootAction::Cd => "cdrom",
            BootAction::Clear => "none",
        }
    }

    /// Redfish `Boot/BootSourceOverrideTarget`; `None` means clear the override.
    pub fn redfish_target(self) -> Option<&'static str> {
        match self {
            BootAction::Pxe => Some("Pxe"),
            BootAction::Disk => Some("Hdd"),
            BootAction::Bios => Some("BiosSetup"),
            BootAction::Cd => Some("Cd"),
            BootAction::Clear => None,
        }
    }

    /// The boot override to optimistically display right after issuing (the next
    /// poll confirms).
    pub fn optimistic_boot(self) -> BootOverride {
        match self {
            BootAction::Pxe => BootOverride::Pxe,
            BootAction::Disk => BootOverride::Disk,
            BootAction::Bios => BootOverride::Bios,
            BootAction::Cd => BootOverride::Cd,
            BootAction::Clear => BootOverride::None,
        }
    }
}

/// One entry from the BMC's System Event Log (SEL). `severity` reuses [`Health`]
/// for colouring (Ok/Warning/Critical, Unknown when undetermined).
#[derive(Debug, Clone)]
pub struct SelEntry {
    /// Record id as the BMC reports it (decimal or hex).
    pub id: String,
    /// Timestamp text (may be "Pre-Init" / blank when the BMC clock is unset).
    pub when: String,
    /// Sensor name the event came from.
    pub sensor: String,
    /// Human-readable event description (including assert/deassert direction).
    pub text: String,
    pub severity: Health,
}

/// A BMC user account (for the detail view's Users tab).
#[derive(Debug, Clone)]
pub struct BmcUser {
    /// Slot/account id.
    pub id: String,
    pub name: String,
    /// Channel privilege limit / role (e.g. ADMINISTRATOR, OPERATOR, USER).
    pub privilege: String,
    /// Whether the account is enabled (IPMI messaging on / Redfish Enabled).
    pub enabled: bool,
}

/// An ordered list of `(label, value)` facts for the detail view's Overview tab.
pub type Overview = Vec<(String, String)>;

/// Result of a power/liveness poll (the frequent one). Beyond power, the single
/// `chassis status` call also yields fault flags and the identify-LED state for
/// free; the boot override is one extra cheap call.
#[derive(Debug, Clone)]
pub struct PowerPoll {
    pub reachable: bool,
    pub power: PowerState,
    /// Any chassis fault flag set (power/cooling/drive/overload).
    pub fault: bool,
    /// Identify (locate) LED is on or blinking.
    pub identify: bool,
    pub boot: BootOverride,
    /// Static fields, only set when the poll was asked to fetch them (once).
    pub serial: Option<String>,
    pub mac: Option<String>,
    pub latency: Duration,
    pub error: Option<String>,
}

impl PowerPoll {
    pub fn reachable() -> Self {
        Self {
            reachable: true,
            power: PowerState::Unknown,
            fault: false,
            identify: false,
            boot: BootOverride::None,
            serial: None,
            mac: None,
            latency: Duration::ZERO,
            error: None,
        }
    }

    pub fn unreachable(err: impl Into<String>) -> Self {
        Self {
            reachable: false,
            power: PowerState::Unknown,
            fault: false,
            identify: false,
            boot: BootOverride::None,
            serial: None,
            mac: None,
            latency: Duration::ZERO,
            error: Some(err.into()),
        }
    }
}

/// Poll power/boot-state (and, when `fetch_static`, the serial + BMC MAC).
/// `demo` forces the mock backend. Bounded by [`POLL_TIMEOUT`].
pub async fn poll_power(machine: &Machine, demo: bool, fetch_static: bool) -> PowerPoll {
    let start = Instant::now();

    let work = async {
        if demo {
            mock::poll_power(machine).await
        } else {
            match machine.protocol {
                Protocol::Ipmi => ipmi::poll_power(machine, fetch_static).await,
                Protocol::Redfish => redfish::poll_power(machine).await,
            }
        }
    };

    let mut poll = match tokio::time::timeout(POLL_TIMEOUT, work).await {
        Ok(p) => p,
        Err(_) => PowerPoll::unreachable(format!("timeout after {}s", POLL_TIMEOUT.as_secs())),
    };

    // Backends that don't simulate their own latency report zero; measure it.
    if poll.latency.is_zero() {
        poll.latency = start.elapsed();
    }
    poll
}

/// Poll sensor health (the slow one). `None` means it couldn't be determined —
/// the caller keeps the previously known health rather than clobbering it.
pub async fn poll_health(machine: &Machine, demo: bool) -> Option<Health> {
    let work = async {
        if demo {
            mock::poll_health(machine).await
        } else {
            match machine.protocol {
                Protocol::Ipmi => ipmi::poll_health(machine).await,
                Protocol::Redfish => redfish::poll_health(machine).await,
            }
        }
    };

    tokio::time::timeout(POLL_TIMEOUT, work)
        .await
        .unwrap_or(None)
}

/// Gather assorted system facts for the detail view's Overview tab (on demand).
pub async fn poll_overview(machine: &Machine, demo: bool) -> Result<Overview, String> {
    let work = async {
        if demo {
            mock::overview(machine).await
        } else {
            match machine.protocol {
                Protocol::Ipmi => ipmi::overview(machine).await,
                Protocol::Redfish => redfish::overview(machine).await,
            }
        }
    };

    match tokio::time::timeout(POLL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(format!("timeout after {}s", POLL_TIMEOUT.as_secs())),
    }
}

/// List the BMC's user accounts with their privileges (on demand).
pub async fn poll_users(machine: &Machine, demo: bool) -> Result<Vec<BmcUser>, String> {
    let work = async {
        if demo {
            mock::users(machine).await
        } else {
            match machine.protocol {
                Protocol::Ipmi => ipmi::users(machine).await,
                Protocol::Redfish => redfish::users(machine).await,
            }
        }
    };

    match tokio::time::timeout(POLL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(format!("timeout after {}s", POLL_TIMEOUT.as_secs())),
    }
}

/// Read the BMC's System Event Log (newest first). Fetched on demand when the
/// detail view opens — not on a polling cadence. Bounded by [`POLL_TIMEOUT`].
pub async fn poll_log(machine: &Machine, demo: bool) -> Result<Vec<SelEntry>, String> {
    let work = async {
        if demo {
            mock::sel_list(machine).await
        } else {
            match machine.protocol {
                Protocol::Ipmi => ipmi::sel_list(machine).await,
                Protocol::Redfish => redfish::sel_list(machine).await,
            }
        }
    };

    match tokio::time::timeout(POLL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(format!("timeout after {}s", POLL_TIMEOUT.as_secs())),
    }
}

/// Set a boot override. `persistent` sticks it across reboots (IPMI
/// `options=persistent` / Redfish `Continuous`); otherwise it's next-boot-only
/// (`Once`). `demo` no-ops successfully. Bounded by [`POLL_TIMEOUT`].
pub async fn set_boot(
    machine: &Machine,
    demo: bool,
    action: BootAction,
    persistent: bool,
) -> Result<(), String> {
    let work = async {
        if demo {
            mock::set_boot(machine, action).await
        } else {
            match machine.protocol {
                Protocol::Ipmi => ipmi::set_boot(machine, action, persistent).await,
                Protocol::Redfish => redfish::set_boot(machine, action, persistent).await,
            }
        }
    };

    match tokio::time::timeout(POLL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(format!("timeout after {}s", POLL_TIMEOUT.as_secs())),
    }
}

/// Issue a power control action. `demo` no-ops successfully. Bounded by
/// [`POLL_TIMEOUT`].
pub async fn power_action(
    machine: &Machine,
    demo: bool,
    action: PowerAction,
) -> Result<(), String> {
    let work = async {
        if demo {
            mock::power(machine, action).await
        } else {
            match machine.protocol {
                Protocol::Ipmi => ipmi::power(machine, action).await,
                Protocol::Redfish => redfish::power(machine, action).await,
            }
        }
    };

    match tokio::time::timeout(POLL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(format!("timeout after {}s", POLL_TIMEOUT.as_secs())),
    }
}
