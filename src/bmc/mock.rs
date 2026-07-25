//! Mock backend for `--demo` — deterministic fake statuses, no hardware.
//!
//! Values are derived from the host string so a given demo machine always
//! renders the same way (stable for screenshots / agent captures), while the
//! set as a whole exercises every visual state: on/off, ok/warn/crit, slow,
//! unreachable, and MAC-mismatch rows.

use std::time::Duration;

use super::{
    BmcUser, BootAction, BootOverride, Health, Overview, PowerAction, PowerPoll, PowerState,
    SelEntry,
};
use crate::inventory::Machine;

fn seed(machine: &Machine) -> u32 {
    machine
        .host
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
}

pub async fn poll_power(machine: &Machine) -> PowerPoll {
    let seed = seed(machine);

    // A small variable delay so the initial "polling" state is briefly visible.
    tokio::time::sleep(Duration::from_millis(150 + (seed % 600) as u64)).await;

    // One in five is unreachable, to exercise that path.
    if seed.is_multiple_of(5) {
        return PowerPoll::unreachable("no route to host (demo)");
    }

    let mut poll = PowerPoll::reachable();
    poll.power = if seed.is_multiple_of(3) {
        PowerState::Off
    } else {
        PowerState::On
    };
    poll.serial = Some(format!("OD{:06}S", seed % 1_000_000));
    // BMC-reported MAC. Usually matches the configured MAC; for one bucket
    // return a wrong one so the "MAC MISMATCH" warning is exercised.
    poll.mac = if seed.is_multiple_of(8) {
        Some("00:00:00:00:00:00".to_string())
    } else {
        machine.mac.clone()
    };
    // Make some rows look "slow" (latency over the 10s threshold) without
    // actually sleeping that long.
    poll.latency = if seed.is_multiple_of(6) {
        Duration::from_secs(12)
    } else {
        Duration::from_millis(20 + (seed % 80) as u64)
    };
    // Sprinkle in chassis faults, identify LEDs, and boot overrides for variety.
    poll.fault = seed.is_multiple_of(4);
    poll.identify = seed.is_multiple_of(9);
    poll.boot = match seed % 7 {
        0 => BootOverride::Pxe,
        3 => BootOverride::Disk,
        _ => BootOverride::None,
    };
    poll
}

pub async fn poll_health(machine: &Machine) -> Option<Health> {
    let seed = seed(machine);
    Some(match seed % 7 {
        0 => Health::Critical,
        1 | 2 => Health::Warning,
        _ => Health::Ok,
    })
}

/// Pretend to issue a power action — always succeeds after a short delay.
pub async fn power(_machine: &Machine, _action: PowerAction) -> Result<(), String> {
    tokio::time::sleep(Duration::from_millis(250)).await;
    Ok(())
}

/// Pretend to set a boot override — always succeeds after a short delay.
pub async fn set_boot(_machine: &Machine, _action: BootAction) -> Result<(), String> {
    tokio::time::sleep(Duration::from_millis(250)).await;
    Ok(())
}

/// Synthesize deterministic Overview facts for `--demo`.
pub async fn overview(machine: &Machine) -> Result<Overview, String> {
    let seed = seed(machine);
    tokio::time::sleep(Duration::from_millis(100 + (seed % 250) as u64)).await;
    if seed.is_multiple_of(5) {
        return Err("no route to host (demo)".to_string());
    }
    let power = if seed.is_multiple_of(3) { "off" } else { "on" };
    let models = ["X11DPT-B", "X12STH-F", "H12SSL-i"];
    let cpus = ["2× Xeon Gold 6330", "2× Xeon Silver 4314", "1× EPYC 7443P"];
    let mem = [256, 512, 128, 1024];
    Ok(vec![
        ("Power".into(), power.into()),
        ("Vendor".into(), "Supermicro (demo)".into()),
        (
            "Product".into(),
            models[seed as usize % models.len()].into(),
        ),
        ("Serial".into(), format!("OD{:06}S", seed % 1_000_000)),
        (
            "BMC Firmware".into(),
            format!("1.{}.{}", seed % 9, seed % 90),
        ),
        ("BIOS".into(), format!("3.{}", seed % 6)),
        ("IPMI Version".into(), "2.0".into()),
        ("CPU".into(), cpus[seed as usize % cpus.len()].into()),
        (
            "Memory".into(),
            format!("{} GiB", mem[seed as usize % mem.len()]),
        ),
        (
            "BMC MAC".into(),
            machine.mac.clone().unwrap_or_else(|| "—".into()),
        ),
        ("BMC IP".into(), machine.host.clone()),
    ])
}

/// Synthesize a deterministic user list for `--demo`.
pub async fn users(machine: &Machine) -> Result<Vec<BmcUser>, String> {
    let seed = seed(machine);
    tokio::time::sleep(Duration::from_millis(100 + (seed % 250) as u64)).await;
    if seed.is_multiple_of(5) {
        return Err("no route to host (demo)".to_string());
    }
    let mut users = vec![
        BmcUser {
            id: "2".into(),
            name: "ADMIN".into(),
            privilege: "ADMINISTRATOR".into(),
            enabled: true,
        },
        BmcUser {
            id: "3".into(),
            name: "operator".into(),
            privilege: "OPERATOR".into(),
            enabled: true,
        },
        BmcUser {
            id: "4".into(),
            name: "monitor".into(),
            privilege: "USER".into(),
            enabled: true,
        },
    ];
    if seed.is_multiple_of(2) {
        users.push(BmcUser {
            id: "5".into(),
            name: "svc-disabled".into(),
            privilege: "NO ACCESS".into(),
            enabled: false,
        });
    }
    Ok(users)
}

/// Synthesize a deterministic SEL for `--demo`. Unreachable demo rows (seed % 5)
/// error, mirroring the list, so the detail view exercises that path too.
pub async fn sel_list(machine: &Machine) -> Result<Vec<SelEntry>, String> {
    let seed = seed(machine);
    tokio::time::sleep(Duration::from_millis(120 + (seed % 300) as u64)).await;
    if seed.is_multiple_of(5) {
        return Err("no route to host (demo)".to_string());
    }

    const SAMPLES: [(Health, &str, &str); 7] = [
        (Health::Ok, "Power Unit", "Power on (asserted)"),
        (
            Health::Warning,
            "Temperature",
            "Upper Non-critical going high (asserted)",
        ),
        (
            Health::Critical,
            "Power Supply",
            "Failure detected (asserted)",
        ),
        (Health::Ok, "System Event", "OEM boot event (asserted)"),
        (Health::Critical, "Memory", "Uncorrectable ECC (asserted)"),
        (
            Health::Warning,
            "Fan",
            "Lower Non-critical going low (asserted)",
        ),
        (
            Health::Ok,
            "Temperature",
            "Upper Critical going high (deasserted)",
        ),
    ];

    let count = 5 + (seed % 10) as usize;
    let mut entries: Vec<SelEntry> = (0..count)
        .map(|i| {
            let (severity, sensor, text) = SAMPLES[(seed as usize + i) % SAMPLES.len()];
            SelEntry {
                id: format!("{}", i + 1),
                when: format!(
                    "06/{:02}/2026 {:02}:{:02}:{:02}",
                    1 + (i % 27),
                    (8 + i) % 24,
                    (i * 7) % 60,
                    (i * 13) % 60
                ),
                sensor: format!("{sensor} #0x{:02x}", 0x30 + i),
                text: text.to_string(),
                severity,
            }
        })
        .collect();
    entries.reverse(); // newest first
    Ok(entries)
}
