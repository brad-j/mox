//! Domain types. `Resource` mirrors the raw `pvesh /cluster/resources` JSON;
//! `Vm` is the cleaned-up view the rest of the app works with.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

/// Raw row from `pvesh get /cluster/resources --type vm --output-format json`.
#[derive(Debug, Deserialize)]
pub struct Resource {
    pub vmid: Option<u32>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub maxmem: Option<u64>,
    pub mem: Option<u64>,
    pub maxdisk: Option<u64>,
    pub maxcpu: Option<f64>,
    pub cpu: Option<f64>,
    pub uptime: Option<u64>,
    /// 1 for templates; may arrive as int, bool, or string depending on version.
    pub template: Option<serde_json::Value>,
    #[serde(rename = "type")]
    pub rtype: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Vm {
    pub vmid: u32,
    pub name: String,
    pub status: String,
    pub is_template: bool,
    pub mem: u64,
    pub maxmem: u64,
    /// Provisioned disk size in bytes (from `maxdisk`).
    pub maxdisk: u64,
    /// Fraction of total CPU in use (0.0..=1.0), as reported by Proxmox.
    pub cpu: f64,
    /// Assigned virtual CPUs (Proxmox reports this as a float, e.g. `2.0`).
    pub maxcpu: f64,
    pub uptime: u64,
}

impl From<Resource> for Vm {
    fn from(r: Resource) -> Self {
        let is_template = match &r.template {
            Some(v) => {
                v.as_i64() == Some(1) || v.as_bool() == Some(true) || v.as_str() == Some("1")
            }
            None => false,
        };
        let status = if is_template {
            "template".to_string()
        } else {
            r.status.unwrap_or_else(|| "unknown".to_string())
        };
        Vm {
            vmid: r.vmid.unwrap_or(0),
            name: r.name.unwrap_or_default(),
            status,
            is_template,
            mem: r.mem.unwrap_or(0),
            maxmem: r.maxmem.unwrap_or(0),
            maxdisk: r.maxdisk.unwrap_or(0),
            cpu: r.cpu.unwrap_or(0.0),
            maxcpu: r.maxcpu.unwrap_or(0.0),
            uptime: r.uptime.unwrap_or(0),
        }
    }
}

impl Vm {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }

    pub fn mem_pct(&self) -> f64 {
        if self.maxmem == 0 {
            0.0
        } else {
            self.mem as f64 / self.maxmem as f64 * 100.0
        }
    }

    pub fn cpu_pct(&self) -> f64 {
        self.cpu * 100.0
    }

    /// Compact human uptime, e.g. `3d4h`, `2h9m`, `12m`, or `—` when stopped.
    pub fn uptime_human(&self) -> String {
        if self.uptime == 0 {
            return "—".to_string();
        }
        let s = self.uptime;
        let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
        if d > 0 {
            format!("{d}d{h}h")
        } else if h > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{m}m")
        }
    }

    /// Provisioned disk as e.g. `20.0 GiB`, or `—` when unknown.
    pub fn disk_human(&self) -> String {
        bytes_gib(self.maxdisk)
    }

    /// Assigned memory (the `maxmem` ceiling) as e.g. `4.0 GiB`.
    pub fn mem_total_human(&self) -> String {
        bytes_gib(self.maxmem)
    }

    /// Currently-used memory as e.g. `1.5 GiB`.
    pub fn mem_used_human(&self) -> String {
        bytes_gib(self.mem)
    }
}

/// Format a byte count as GiB with one decimal, or `—` for zero.
fn bytes_gib(bytes: u64) -> String {
    if bytes == 0 {
        return "—".to_string();
    }
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// LAN and Tailscale IPv4 addresses resolved from the guest agent.
#[derive(Debug, Clone, Default)]
pub struct GuestIps {
    pub lan: Option<String>,
    pub tailscale: Option<String>,
}

/// One interface from `qm guest cmd VMID network-get-interfaces`.
#[derive(Debug, Deserialize)]
struct GuestIface {
    #[serde(rename = "ip-addresses", default)]
    ip_addresses: Vec<GuestAddr>,
}

#[derive(Debug, Deserialize)]
struct GuestAddr {
    #[serde(rename = "ip-address-type")]
    ip_address_type: Option<String>,
    #[serde(rename = "ip-address")]
    ip_address: Option<String>,
}

/// Tailscale's CGNAT range is `100.64.0.0/10` — first octet 100, second 64..=127.
fn is_tailscale(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped exactly like real `qm guest cmd VMID network-get-interfaces` output:
    // loopback + a LAN interface + a Tailscale (CGNAT) interface, with IPv6 mixed in.
    const IFACES: &str = r#"[
      {"name":"lo","ip-addresses":[
        {"ip-address":"127.0.0.1","ip-address-type":"ipv4","prefix":8},
        {"ip-address":"::1","ip-address-type":"ipv6","prefix":128}]},
      {"name":"eth0","ip-addresses":[
        {"ip-address":"192.168.3.170","ip-address-type":"ipv4","prefix":24},
        {"ip-address":"fe80::be24:11ff:fe14:da9f","ip-address-type":"ipv6","prefix":64}]},
      {"name":"tailscale0","ip-addresses":[
        {"ip-address":"100.101.102.103","ip-address-type":"ipv4","prefix":32}]}
    ]"#;

    #[test]
    fn classifies_lan_and_tailscale_ips() {
        let ips = GuestIps::from_interfaces_json(IFACES).unwrap();
        assert_eq!(ips.lan.as_deref(), Some("192.168.3.170"));
        assert_eq!(ips.tailscale.as_deref(), Some("100.101.102.103"));
    }

    #[test]
    fn skips_loopback_and_reports_none_without_addresses() {
        let ips = GuestIps::from_interfaces_json(
            r#"[{"name":"lo","ip-addresses":[{"ip-address":"127.0.0.1","ip-address-type":"ipv4","prefix":8}]}]"#,
        )
        .unwrap();
        assert_eq!(ips.lan, None);
        assert_eq!(ips.tailscale, None);
    }

    #[test]
    fn tailscale_range_boundary() {
        // 100.64.0.0/10 spans 100.64.x – 100.127.x; 100.128 and 100.63 are LAN.
        assert!(is_tailscale("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale("100.127.255.255".parse().unwrap()));
        assert!(!is_tailscale("100.63.0.1".parse().unwrap()));
        assert!(!is_tailscale("100.128.0.1".parse().unwrap()));
    }
}

impl GuestIps {
    /// Parse `network-get-interfaces` JSON, picking the first non-loopback IPv4
    /// outside the CGNAT range as the LAN address and the first inside it as the
    /// Tailscale address — mirroring the `pve` bash tool's `vm_guest_ipv4`.
    pub fn from_interfaces_json(json: &str) -> anyhow::Result<GuestIps> {
        let ifaces: Vec<GuestIface> = serde_json::from_str(json)?;
        let mut out = GuestIps::default();
        for iface in ifaces {
            for addr in iface.ip_addresses {
                if addr.ip_address_type.as_deref() != Some("ipv4") {
                    continue;
                }
                let Some(raw) = addr.ip_address.as_deref() else {
                    continue;
                };
                let Ok(ip) = raw.parse::<Ipv4Addr>() else {
                    continue;
                };
                if ip.is_loopback() {
                    continue;
                }
                if is_tailscale(ip) {
                    if out.tailscale.is_none() {
                        out.tailscale = Some(raw.to_string());
                    }
                } else if out.lan.is_none() {
                    out.lan = Some(raw.to_string());
                }
            }
        }
        Ok(out)
    }
}
