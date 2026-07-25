//! Redfish backend (scaffold) — queries the BMC's REST API over HTTPS.
//!
//! Secondary to IPMI for now: it fetches the first system and reads PowerState /
//! SerialNumber / Status.Health. Self-signed BMC certificates are accepted when
//! the machine is marked `insecure`. Both poll types reuse the same system GET
//! (it's cheap), so unlike IPMI there's no real power/health cost asymmetry.

use super::{
    BmcUser, BootAction, BootOverride, Health, Overview, PowerAction, PowerPoll, PowerState,
    SelEntry,
};
use crate::inventory::Machine;

pub async fn poll_power(machine: &Machine) -> PowerPoll {
    match fetch_system(machine).await {
        Ok(sys) => {
            let mut poll = PowerPoll::reachable();
            poll.power = parse_power(&sys);
            poll.fault = matches!(
                sys.pointer("/Status/Health").and_then(|v| v.as_str()),
                Some("Warning") | Some("Critical")
            );
            poll.identify = sys
                .get("LocationIndicatorActive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || matches!(
                    sys.get("IndicatorLED").and_then(|v| v.as_str()),
                    Some("Lit") | Some("Blinking")
                );
            poll.boot = parse_boot(&sys);
            poll.serial = sys
                .get("SerialNumber")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // TODO: the BMC MAC lives under /redfish/v1/Managers/<id>/EthernetInterfaces,
            // not on the System — left unfetched, so MAC verification is IPMI-only for now.
            poll
        }
        Err(e) => PowerPoll::unreachable(e),
    }
}

pub async fn poll_health(machine: &Machine) -> Option<Health> {
    fetch_system(machine)
        .await
        .ok()
        .map(|sys| parse_health(&sys))
}

/// Issue a power action via the system's `ComputerSystem.Reset` action.
pub async fn power(machine: &Machine, action: PowerAction) -> Result<(), String> {
    let port = machine.port.map(|p| format!(":{p}")).unwrap_or_default();
    let base = format!("{}://{}{}", machine.scheme, machine.host, port);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(machine.insecure)
        .build()
        .map_err(|e| e.to_string())?;

    let sys = fetch_system(machine).await?;
    let target = sys
        .pointer("/Actions/#ComputerSystem.Reset/target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "no reset action on system".to_string())?;

    client
        .post(format!("{base}{target}"))
        .basic_auth(&machine.username, Some(&machine.password))
        .json(&serde_json::json!({ "ResetType": action.redfish_reset_type() }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Set a boot override by PATCHing the system's `Boot` object. `persistent`
/// uses `Continuous` (sticks across reboots) instead of `Once`.
pub async fn set_boot(
    machine: &Machine,
    action: BootAction,
    persistent: bool,
) -> Result<(), String> {
    let port = machine.port.map(|p| format!(":{p}")).unwrap_or_default();
    let base = format!("{}://{}{}", machine.scheme, machine.host, port);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(machine.insecure)
        .build()
        .map_err(|e| e.to_string())?;

    let sys = fetch_system(machine).await?;
    let target = sys
        .get("@odata.id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "system has no @odata.id".to_string())?;

    let enabled = if persistent { "Continuous" } else { "Once" };
    let boot = match action.redfish_target() {
        Some(t) => serde_json::json!({
            "BootSourceOverrideEnabled": enabled,
            "BootSourceOverrideTarget": t,
        }),
        None => serde_json::json!({ "BootSourceOverrideEnabled": "Disabled" }),
    };

    client
        .patch(format!("{base}{target}"))
        .basic_auth(&machine.username, Some(&machine.password))
        .json(&serde_json::json!({ "Boot": boot }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Gather Overview facts from the Redfish system object.
pub async fn overview(machine: &Machine) -> Result<Overview, String> {
    let sys = fetch_system(machine).await?;
    let mut ov: Overview = Vec::new();
    let mut push = |label: &str, val: Option<&str>| {
        if let Some(v) = val {
            let v = v.trim();
            if !v.is_empty() {
                ov.push((label.to_string(), v.to_string()));
            }
        }
    };

    push("Power", sys.get("PowerState").and_then(|v| v.as_str()));
    push("Vendor", sys.get("Manufacturer").and_then(|v| v.as_str()));
    push("Model", sys.get("Model").and_then(|v| v.as_str()));
    push("Serial", sys.get("SerialNumber").and_then(|v| v.as_str()));
    push("SKU", sys.get("SKU").and_then(|v| v.as_str()));
    push("BIOS", sys.get("BiosVersion").and_then(|v| v.as_str()));
    push(
        "Health",
        sys.pointer("/Status/Health").and_then(|v| v.as_str()),
    );
    push(
        "CPU Model",
        sys.pointer("/ProcessorSummary/Model")
            .and_then(|v| v.as_str()),
    );
    if let Some(n) = sys
        .pointer("/ProcessorSummary/Count")
        .and_then(|v| v.as_u64())
    {
        ov.push(("CPU Count".to_string(), n.to_string()));
    }
    if let Some(g) = sys
        .pointer("/MemorySummary/TotalSystemMemoryGiB")
        .and_then(|v| v.as_f64())
    {
        ov.push(("Memory".to_string(), format!("{g} GiB")));
    }
    Ok(ov)
}

/// List accounts via `AccountService/Accounts` (fetch each member for its role).
pub async fn users(machine: &Machine) -> Result<Vec<BmcUser>, String> {
    let port = machine.port.map(|p| format!(":{p}")).unwrap_or_default();
    let base = format!("{}://{}{}", machine.scheme, machine.host, port);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(machine.insecure)
        .build()
        .map_err(|e| e.to_string())?;
    let get = |url: String| {
        client
            .get(url)
            .basic_auth(&machine.username, Some(&machine.password))
            .send()
    };

    let coll: serde_json::Value = get(format!("{base}/redfish/v1/AccountService/Accounts"))
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let members = coll
        .get("Members")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "Accounts collection has no Members".to_string())?;

    let mut users = Vec::new();
    for m in members {
        let Some(id) = m.get("@odata.id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(acct) = get(format!("{base}{id}")).await else {
            continue;
        };
        let Ok(acct) = acct.json::<serde_json::Value>().await else {
            continue;
        };
        let name = acct.get("UserName").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        users.push(BmcUser {
            id: acct
                .get("Id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: name.to_string(),
            privilege: acct
                .get("RoleId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            enabled: acct
                .get("Enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    Ok(users)
}

/// Read the SEL via the system's `LogServices/SEL/Entries` collection.
pub async fn sel_list(machine: &Machine) -> Result<Vec<SelEntry>, String> {
    let port = machine.port.map(|p| format!(":{p}")).unwrap_or_default();
    let base = format!("{}://{}{}", machine.scheme, machine.host, port);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(machine.insecure)
        .build()
        .map_err(|e| e.to_string())?;

    let sys = fetch_system(machine).await?;
    let member = sys
        .get("@odata.id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "system has no @odata.id".to_string())?;

    let coll: serde_json::Value = client
        .get(format!("{base}{member}/LogServices/SEL/Entries"))
        .basic_auth(&machine.username, Some(&machine.password))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let members = coll
        .get("Members")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "SEL collection has no Members".to_string())?;

    let entries = members
        .iter()
        .map(|m| {
            let severity = match m.get("Severity").and_then(|v| v.as_str()) {
                Some("OK") => Health::Ok,
                Some("Warning") => Health::Warning,
                Some("Critical") => Health::Critical,
                _ => Health::Unknown,
            };
            SelEntry {
                id: m
                    .get("Id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                when: m
                    .get("Created")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                sensor: m
                    .get("SensorType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                text: m
                    .get("Message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                severity,
            }
        })
        .collect();
    Ok(entries)
}

/// GET the first system object from the Redfish service.
async fn fetch_system(machine: &Machine) -> Result<serde_json::Value, String> {
    let port = machine.port.map(|p| format!(":{p}")).unwrap_or_default();
    let base = format!("{}://{}{}", machine.scheme, machine.host, port);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(machine.insecure)
        .build()
        .map_err(|e| e.to_string())?;

    let get = |url: String| {
        client
            .get(url)
            .basic_auth(&machine.username, Some(&machine.password))
            .send()
    };

    let coll: serde_json::Value = get(format!("{base}/redfish/v1/Systems"))
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let member = coll
        .pointer("/Members/0/@odata.id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "no systems in Redfish collection".to_string())?;

    get(format!("{base}{member}"))
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

fn parse_power(sys: &serde_json::Value) -> PowerState {
    match sys.get("PowerState").and_then(|v| v.as_str()) {
        Some("On") => PowerState::On,
        Some("Off") => PowerState::Off,
        _ => PowerState::Unknown,
    }
}

fn parse_health(sys: &serde_json::Value) -> Health {
    match sys.pointer("/Status/Health").and_then(|v| v.as_str()) {
        Some("OK") => Health::Ok,
        Some("Warning") => Health::Warning,
        Some("Critical") => Health::Critical,
        _ => Health::Unknown,
    }
}

fn parse_boot(sys: &serde_json::Value) -> BootOverride {
    // No override unless BootSourceOverrideEnabled is Once/Continuous.
    match sys
        .pointer("/Boot/BootSourceOverrideEnabled")
        .and_then(|v| v.as_str())
    {
        Some("Once") | Some("Continuous") => {}
        _ => return BootOverride::None,
    }
    match sys
        .pointer("/Boot/BootSourceOverrideTarget")
        .and_then(|v| v.as_str())
    {
        Some("Pxe") => BootOverride::Pxe,
        Some("Hdd") => BootOverride::Disk,
        Some("BiosSetup") => BootOverride::Bios,
        Some("Cd") => BootOverride::Cd,
        None | Some("None") => BootOverride::None,
        Some(_) => BootOverride::Other,
    }
}
