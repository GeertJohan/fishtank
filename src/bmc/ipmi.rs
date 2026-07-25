//! IPMI backend — shells out to `ipmitool` (lanplus).
//!
//! The password is passed via the `IPMI_PASSWORD` env var together with `-E`,
//! so it never appears in the process arguments (`ps`/argv). This mirrors the
//! existing `fundament-poc` `bmc-normalize.sh` access pattern.
//!
//! No per-call timeout: the whole poll is bounded by `bmc::POLL_TIMEOUT`, and
//! `kill_on_drop` ensures a hung `ipmitool` is killed when that fires.
//!
//! Power (`chassis power status`) is cheap and runs frequently; health
//! (`sdr list`) walks every sensor over the network and is slow, so it runs on
//! a separate, slower cadence — see [`poll_power`] vs [`poll_health`].

use std::process::Stdio;

use tokio::process::Command;

use super::{
    BmcUser, BootAction, BootOverride, Health, Overview, PowerAction, PowerPoll, PowerState,
    SelEntry,
};
use crate::inventory::Machine;

/// LAN channel to read the BMC MAC from. 1 is the Supermicro dedicated IPMI LAN
/// (matches `bmc-normalize.sh`'s default).
const LAN_CHANNEL: &str = "1";

/// Run one `ipmitool` subcommand, returning stdout or a short error string.
async fn ipmitool(machine: &Machine, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("ipmitool");
    cmd.env("IPMI_PASSWORD", &machine.password)
        .arg("-I")
        .arg("lanplus")
        .arg("-H")
        .arg(&machine.host)
        .arg("-U")
        .arg(&machine.username)
        .arg("-E") // read password from IPMI_PASSWORD
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("failed to run ipmitool: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr
            .trim()
            .lines()
            .next()
            .unwrap_or("ipmitool error")
            .to_string();
        return Err(msg);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Power/boot-state probe. A single `chassis status` doubles as the reachability
/// check and yields power, fault flags and the identify-LED state; the boot
/// override is one more cheap call. Static fields (serial via `fru`, BMC MAC via
/// `lan print`) are fetched only when `fetch_static` is set — the caller asks once.
pub async fn poll_power(machine: &Machine, fetch_static: bool) -> PowerPoll {
    let status_out = match ipmitool(machine, &["chassis", "status"]).await {
        Ok(o) => o,
        Err(e) => return PowerPoll::unreachable(e),
    };

    let mut poll = PowerPoll::reachable();
    poll.power = parse_power(&status_out);
    poll.fault = parse_fault(&status_out);
    poll.identify = parse_identify(&status_out);

    // Boot override (provisioning-relevant) — best-effort.
    if let Ok(boot) = ipmitool(machine, &["chassis", "bootparam", "get", "5"]).await {
        tracing::debug!("{} bootparam 5:\n{}", machine.name, boot.trim_end());
        poll.boot = parse_boot(&boot);
    }

    if fetch_static {
        if let Ok(fru) = ipmitool(machine, &["fru"]).await {
            poll.serial = parse_serial(&fru);
        }
        if let Ok(lan) = ipmitool(machine, &["lan", "print", LAN_CHANNEL]).await {
            poll.mac = parse_field(&lan, "MAC Address");
        }
    }

    poll
}

/// Sensor health via `sdr list` (the slow call). `None` if it couldn't be read.
pub async fn poll_health(machine: &Machine) -> Option<Health> {
    match ipmitool(machine, &["sdr", "list"]).await {
        Ok(sdr) => Some(parse_health(&sdr)),
        Err(_) => None,
    }
}

/// Issue a power action via `ipmitool chassis power <verb>`.
pub async fn power(machine: &Machine, action: PowerAction) -> Result<(), String> {
    ipmitool(machine, &["chassis", "power", action.ipmi_verb()])
        .await
        .map(|_| ())
}

/// Gather Overview facts from `mc info`, `fru`, `lan print` and `chassis status`.
/// Each source is best-effort — a failing call just omits its fields.
pub async fn overview(machine: &Machine) -> Result<Overview, String> {
    // Reachability gate: if `mc info` fails the BMC is unreachable.
    let mc = ipmitool(machine, &["mc", "info"]).await?;
    let fru = ipmitool(machine, &["fru"]).await.unwrap_or_default();
    let lan = ipmitool(machine, &["lan", "print", LAN_CHANNEL])
        .await
        .unwrap_or_default();
    let chassis = ipmitool(machine, &["chassis", "status"])
        .await
        .unwrap_or_default();

    let mut ov: Overview = Vec::new();
    let mut push = |label: &str, val: Option<String>| {
        if let Some(v) = val {
            let v = v.trim().to_string();
            if !v.is_empty() {
                ov.push((label.to_string(), v));
            }
        }
    };

    push("Power", Some(power_text(parse_power(&chassis))));
    push(
        "Vendor",
        parse_field(&fru, "Product Manufacturer").or_else(|| parse_field(&mc, "Manufacturer Name")),
    );
    push("Product", parse_field(&fru, "Product Name"));
    push("Board", parse_field(&fru, "Board Product"));
    push("Serial", parse_serial(&fru));
    push("Part Number", parse_field(&fru, "Product Part Number"));
    push("BMC Firmware", parse_field(&mc, "Firmware Revision"));
    push("IPMI Version", parse_field(&mc, "IPMI Version"));
    push("BMC MAC", parse_field(&lan, "MAC Address"));
    push("BMC IP", parse_field(&lan, "IP Address"));
    push("BMC Subnet", parse_field(&lan, "Subnet Mask"));
    push("BMC Gateway", parse_field(&lan, "Default Gateway IP"));
    push("VLAN", parse_field(&lan, "802.1q VLAN ID"));

    Ok(ov)
}

/// List BMC user accounts via `ipmitool user list <channel>`.
pub async fn users(machine: &Machine) -> Result<Vec<BmcUser>, String> {
    let out = ipmitool(machine, &["user", "list", LAN_CHANNEL]).await?;
    Ok(parse_users(&out))
}

/// Deactivate any stale Serial-over-LAN session (best-effort pre-flight before
/// `sol activate`, which is exclusive — one session per BMC).
pub async fn sol_deactivate(machine: &Machine) -> Result<(), String> {
    ipmitool(machine, &["sol", "deactivate"]).await.map(|_| ())
}

/// Read the System Event Log via `ipmitool sel elist`, newest first.
pub async fn sel_list(machine: &Machine) -> Result<Vec<SelEntry>, String> {
    let out = ipmitool(machine, &["sel", "elist"]).await?;
    let mut entries = parse_sel(&out);
    // `sel elist` is oldest-first (record order); show newest at the top.
    entries.reverse();
    Ok(entries)
}

/// Set a boot override via `ipmitool chassis bootdev <dev>`. `persistent` adds
/// `options=persistent` so it sticks across reboots (not meaningful when
/// clearing); otherwise it applies to the next boot only.
pub async fn set_boot(
    machine: &Machine,
    action: BootAction,
    persistent: bool,
) -> Result<(), String> {
    let mut args = vec!["chassis", "bootdev", action.ipmi_bootdev()];
    if persistent && !matches!(action, BootAction::Clear) {
        args.push("options=persistent");
    }
    ipmitool(machine, &args).await.map(|_| ())
}

/// Power from `chassis status` ("System Power : on"), falling back to the
/// `chassis power status` phrasing ("Chassis Power is on") for robustness.
fn parse_power(s: &str) -> PowerState {
    if let Some(v) = parse_field(s, "System Power") {
        return match v.trim().to_ascii_lowercase().as_str() {
            "on" => PowerState::On,
            "off" => PowerState::Off,
            _ => PowerState::Unknown,
        };
    }
    let l = s.to_ascii_lowercase();
    if l.contains("is on") {
        PowerState::On
    } else if l.contains("is off") {
        PowerState::Off
    } else {
        PowerState::Unknown
    }
}

/// Any chassis fault flag set, from `chassis status`.
fn parse_fault(s: &str) -> bool {
    const FAULTS: [&str; 5] = [
        "Power Overload",
        "Main Power Fault",
        "Power Control Fault",
        "Drive Fault",
        "Cooling/Fan Fault",
    ];
    FAULTS.iter().any(|k| {
        parse_field(s, k)
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Identify (locate) LED on/blinking, from `chassis status`.
fn parse_identify(s: &str) -> bool {
    parse_field(s, "Chassis Identify State")
        .map(|v| !v.trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

/// Boot override from `chassis bootparam get 5`. The flags live in a bulleted
/// block (`   - Boot Device Selector : Force PXE`), and a separate line marks
/// whether the flags are valid. If the BMC reports the flags invalid (e.g. the
/// one-time override timed out or was consumed by a boot), there's no active
/// override regardless of the leftover selector.
fn parse_boot(s: &str) -> BootOverride {
    if s.to_ascii_lowercase().contains("boot flag invalid") {
        return BootOverride::None;
    }
    match parse_field(s, "Boot Device Selector") {
        None => BootOverride::None,
        Some(sel) => {
            let s = sel.to_ascii_lowercase();
            if s.contains("no override") {
                BootOverride::None
            } else if s.contains("pxe") {
                BootOverride::Pxe
            } else if s.contains("hard-drive") || s.contains("hard drive") {
                BootOverride::Disk
            } else if s.contains("bios") {
                BootOverride::Bios
            } else if s.contains("cd") || s.contains("dvd") {
                BootOverride::Cd
            } else {
                BootOverride::Other
            }
        }
    }
}

/// Prefer the product serial (node identity) over the board serial.
fn parse_serial(fru: &str) -> Option<String> {
    parse_field(fru, "Product Serial").or_else(|| parse_field(fru, "Board Serial"))
}

/// Extract a `Key : Value` field (case-insensitive key, first match). Tolerates
/// a leading bullet on the key, as ipmitool uses for the boot-flags block
/// (`   - Boot Device Selector : Force PXE`).
fn parse_field(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':')
            && k.trim()
                .trim_start_matches(['-', '*', '•'])
                .trim()
                .eq_ignore_ascii_case(key)
        {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn power_text(p: PowerState) -> String {
    match p {
        PowerState::On => "on",
        PowerState::Off => "off",
        PowerState::Unknown => "unknown",
    }
    .to_string()
}

/// Parse `ipmitool user list <ch>` (whitespace-aligned columns):
/// `ID  Name  Callin  Link Auth  IPMI Msg  Channel Priv Limit`.
/// Empty (unused) slots — no name — are skipped. The name column can be absent,
/// so we detect it by whether the token after the id is a boolean.
fn parse_users(out: &str) -> Vec<BmcUser> {
    let mut users = Vec::new();
    for line in out.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 4 {
            continue;
        }
        // First column must be a numeric id (skips the header row).
        if toks[0].parse::<u32>().is_err() {
            continue;
        }
        let id = toks[0].to_string();
        let is_bool = |s: &str| s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false");
        // Name present unless toks[1] is already the first boolean column.
        let (name, rest) = if is_bool(toks[1]) {
            (String::new(), &toks[1..])
        } else {
            (toks[1].to_string(), &toks[2..])
        };
        // rest = [callin, link_auth, ipmi_msg, priv...]; require the 3 booleans.
        if rest.len() < 4 || !is_bool(rest[0]) || !is_bool(rest[1]) || !is_bool(rest[2]) {
            continue;
        }
        if name.is_empty() {
            continue; // unused slot
        }
        let enabled = rest[2].eq_ignore_ascii_case("true"); // IPMI Msg
        let privilege = rest[3..].join(" ");
        users.push(BmcUser {
            id,
            name,
            privilege,
            enabled,
        });
    }
    users
}

/// Parse `sel elist` output (pipe-delimited records). The common shape is
/// `id | date | time | sensor | event | assert/deassert`; pre-init records
/// merge the timestamp into one field. Non-record lines (e.g. "SEL has no
/// entries") have too few fields and are skipped.
fn parse_sel(out: &str) -> Vec<SelEntry> {
    let mut entries = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('|').map(str::trim).collect();
        if parts.len() < 4 {
            continue;
        }
        let id = parts[0].to_string();
        // Map fields by count: 6+ has separate date/time; 5 has a merged
        // timestamp (pre-init); 4 has no direction column.
        let (when, sensor, event, dir) = match parts.len() {
            n if n >= 6 => (
                format!("{} {}", parts[1], parts[2]),
                parts[3].to_string(),
                parts[4].to_string(),
                parts[5],
            ),
            5 => (
                parts[1].to_string(),
                parts[2].to_string(),
                parts[3].to_string(),
                parts[4],
            ),
            _ => (
                parts[1].to_string(),
                parts[2].to_string(),
                parts[3].to_string(),
                "",
            ),
        };
        let text = if dir.is_empty() {
            event.clone()
        } else {
            format!("{event} ({})", dir.to_ascii_lowercase())
        };
        entries.push(SelEntry {
            id,
            when,
            sensor,
            severity: sel_severity(&event, dir),
            text,
        });
    }
    entries
}

/// Best-effort severity from the event text (ipmitool has no clean severity
/// column). A deassertion is a recovery → Ok.
fn sel_severity(event: &str, dir: &str) -> Health {
    if dir.eq_ignore_ascii_case("Deasserted") {
        return Health::Ok;
    }
    let t = event.to_ascii_lowercase();
    if t.contains("non-recoverable")
        || t.contains("critical")
        || t.contains("failure")
        || t.contains("fault")
        || t.contains("uncorrectable")
    {
        Health::Critical
    } else if t.contains("non-critical")
        || t.contains("warning")
        || t.contains("degraded")
        || t.contains("predictive")
    {
        Health::Warning
    } else {
        Health::Unknown
    }
}

/// Derive a single health summary from `sdr list` (pipe-delimited:
/// `name | reading | status`). Takes the worst sensor state.
fn parse_health(sdr: &str) -> Health {
    let mut worst = Health::Ok;
    let mut saw_any = false;
    for line in sdr.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 3 {
            continue;
        }
        let st = parts[2].trim().to_ascii_lowercase();
        let level = match st.as_str() {
            "ok" => Health::Ok,
            "nc" | "lnc" | "unc" | "warning" => Health::Warning,
            "cr" | "lcr" | "ucr" | "nr" | "lnr" | "unr" | "critical" => Health::Critical,
            // "ns" (no reading / not present) and anything unrecognised: ignore.
            _ => continue,
        };
        saw_any = true;
        worst = worst.worst(level);
    }
    if saw_any { worst } else { Health::Unknown }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Realistic `ipmitool chassis bootparam get 5` output: the boot flags are a
    // bulleted block, so the selector key arrives as "   - Boot Device Selector".
    const BOOTPARAM_PXE: &str = "\
Boot parameter version: 1
Boot parameter 5 is valid/unlocked
Boot parameter data: 8004000000
 Boot Flags :
   - Boot Flag Valid
   - Options apply to only next boot
   - BIOS PC Compatible (legacy) boot
   - Boot Device Selector : Force PXE
   - Console Redirection control : System Default";

    const BOOTPARAM_NONE: &str = "\
 Boot Flags :
   - Boot Flag Valid
   - Boot Device Selector : No override";

    const BOOTPARAM_INVALID: &str = "\
 Boot Flags :
   - Boot Flag Invalid
   - Boot Device Selector : Force PXE";

    #[test]
    fn parses_bulleted_boot_selector() {
        assert_eq!(parse_boot(BOOTPARAM_PXE), BootOverride::Pxe);
        assert_eq!(parse_boot(BOOTPARAM_NONE), BootOverride::None);
    }

    #[test]
    fn invalid_flags_mean_no_override() {
        // Selector still reads PXE, but the flags are marked invalid.
        assert_eq!(parse_boot(BOOTPARAM_INVALID), BootOverride::None);
    }

    const SEL_OUT: &str = "\
   1 | 01/15/2024 | 09:12:34 | Power Unit #0xc8 | Power off/down | Asserted
   2 | 01/15/2024 | 09:13:01 | Temperature #0x01 | Upper Critical going high | Asserted
   3 | Pre-Init Time-stamp | Memory #0x12 | Uncorrectable ECC | Asserted
SEL has no more entries";

    #[test]
    fn parses_sel_records_newest_first_after_reverse() {
        let mut e = parse_sel(SEL_OUT);
        assert_eq!(e.len(), 3);
        // parse_sel preserves record order; sel_list reverses for display.
        assert_eq!(e[0].id, "1");
        assert_eq!(e[1].sensor, "Temperature #0x01");
        assert_eq!(e[1].severity, Health::Critical);
        assert_eq!(e[2].when, "Pre-Init Time-stamp");
        assert_eq!(e[2].severity, Health::Critical); // uncorrectable ECC
        assert!(e[0].text.contains("(asserted)"));
        e.reverse();
        assert_eq!(e[0].id, "3");
    }

    const USER_LIST: &str = "\
ID  Name             Callin  Link Auth  IPMI Msg   Channel Priv Limit
1                    true    false      false      Unknown (0x00)
2   ADMIN            true    true       true       ADMINISTRATOR
3   operator         true    true       true       OPERATOR
4                    true    false      false      NO ACCESS";

    #[test]
    fn parses_user_list_skipping_empty_slots() {
        let u = parse_users(USER_LIST);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].id, "2");
        assert_eq!(u[0].name, "ADMIN");
        assert_eq!(u[0].privilege, "ADMINISTRATOR");
        assert!(u[0].enabled);
        assert_eq!(u[1].name, "operator");
        assert_eq!(u[1].privilege, "OPERATOR");
    }

    #[test]
    fn sel_deassert_is_ok() {
        let e = parse_sel(
            "  5 | 01/15/2024 | 10:00:00 | Temperature #0x01 | Upper Critical going high | Deasserted",
        );
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].severity, Health::Ok);
    }

    #[test]
    fn parse_field_handles_plain_and_bulleted_keys() {
        assert_eq!(
            parse_field("System Power : on", "System Power").as_deref(),
            Some("on")
        );
        assert_eq!(
            parse_field(
                "   - Boot Device Selector : Force PXE",
                "Boot Device Selector"
            )
            .as_deref(),
            Some("Force PXE")
        );
    }
}
