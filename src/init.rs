//! `mox init` — interactive setup wizard. Prompts for connection + resource +
//! tailscale settings (prefilled from any existing config), detects storages /
//! bridges when access already works, and writes `~/.config/mox/config` (backing
//! up any existing file). Ports the bash tool's `init_config` / `write_env_file`.

use std::fs;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{self, Config};
use crate::node::Node;

pub fn run() -> Result<()> {
    println!("mox interactive setup");
    println!("=====================");

    let path = config::init_path().context("cannot resolve ~/.config/mox/config (no HOME)")?;
    // Prefill from an existing loadable config, else built-in defaults.
    let mut cfg = Config::load().unwrap_or_else(|_| Config::defaults());

    if path.exists() {
        println!("\nA config already exists at {}.", path.display());
        println!("Continuing backs it up to <config>.bak and writes a new one.");
        if !prompt_yes_no("Continue?", true)? {
            println!("Cancelled.");
            return Ok(());
        }
    }

    println!("\n== Proxmox node connection ==");
    loop {
        cfg.host = prompt("Proxmox node IP or hostname", &cfg.host)?;
        if !cfg.host.trim().is_empty() {
            break;
        }
        println!("Host is required.");
    }
    cfg.user = prompt("SSH user", &cfg.user)?;
    loop {
        let p = prompt("SSH port", &cfg.port.to_string())?;
        match p.parse::<u16>() {
            Ok(n) if n > 0 => {
                cfg.port = n;
                break;
            }
            _ => println!("Invalid port: {p}"),
        }
    }
    let id_default = cfg
        .identity_file
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| format!("{}/.ssh/proxmox/id_ed25519", home()));
    let id = prompt("SSH identity file", &id_default)?;
    cfg.identity_file = if id.is_empty() {
        None
    } else {
        Some(PathBuf::from(id))
    };

    let subnet_default = cfg
        .vm_subnet
        .clone()
        .unwrap_or_else(|| derive_subnet(&cfg.host));
    let subnet = prompt("VM subnet (for ~/.ssh/config)", &subnet_default)?;
    cfg.vm_subnet = if subnet.is_empty() { None } else { Some(subnet) };

    // Node resources — detect them if key-based access already works.
    println!("\n== Node resources ==");
    let probe = Node::new(cfg.clone());
    if let Ok(s) = probe.storages() {
        if !s.is_empty() {
            println!("Detected storages: {}", s.join(", "));
        }
    }
    cfg.storage = prompt("Storage for VM disks", &cfg.storage)?;
    if let Ok(b) = probe.bridges() {
        if !b.is_empty() {
            println!("Detected bridges: {}", b.join(", "));
        }
    }
    cfg.bridge = prompt("Network bridge", &cfg.bridge)?;

    println!("\n== Tailscale (optional) ==");
    if prompt_yes_no("Join new VMs to a tailnet by default?", cfg.tailscale_default)? {
        cfg.tailscale_default = true;
        let k = prompt(
            "Tailscale reusable auth key (tskey-auth-...)",
            cfg.tailscale_authkey.as_deref().unwrap_or(""),
        )?;
        cfg.tailscale_authkey = if k.is_empty() { None } else { Some(k) };
        let d = prompt(
            "Tailnet domain (e.g. example-name.ts.net)",
            cfg.tailnet_domain.as_deref().unwrap_or(""),
        )?;
        cfg.tailnet_domain = if d.is_empty() { None } else { Some(d) };
    } else {
        cfg.tailscale_default = false;
        println!("Tailscale off by default. Toggle it per-VM in the create wizard.");
    }

    // Back up any existing file, then write.
    if path.exists() {
        let bak = backup_path(&path);
        fs::copy(&path, &bak).with_context(|| format!("backing up to {}", bak.display()))?;
        println!("\nBacked up existing config to {}", bak.display());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&path, cfg.to_env_string()).with_context(|| format!("writing {}", path.display()))?;
    set_mode(&path, 0o600);
    println!("Wrote {}", path.display());

    println!("\nNext: `mox doctor`, then `mox template` (if needed), then create VMs.");
    Ok(())
}

fn prompt(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let ans = line.trim();
    Ok(if ans.is_empty() {
        default.to_string()
    } else {
        ans.to_string()
    })
}

fn prompt_yes_no(label: &str, default_yes: bool) -> Result<bool> {
    let ans = prompt(label, if default_yes { "y" } else { "n" })?;
    Ok(matches!(ans.to_lowercase().as_str(), "y" | "yes"))
}

/// `~/.config/mox/config` → `~/.config/mox/config.bak` (append, so hidden names
/// like `.env` become `.env.bak` too).
fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.to_path_buf().into_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

/// Guess a `/24` subnet from an IPv4 host (first three octets); fall back to the
/// documentation range for a hostname.
fn derive_subnet(host: &str) -> String {
    match host.parse::<Ipv4Addr>() {
        Ok(ip) => {
            let o = ip.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        Err(_) => "192.0.2.0/24".to_string(),
    }
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_subnet_from_ip_or_default() {
        assert_eq!(derive_subnet("192.168.3.160"), "192.168.3.0/24");
        assert_eq!(derive_subnet("10.1.2.3"), "10.1.2.0/24");
        assert_eq!(derive_subnet("pve.example.com"), "192.0.2.0/24");
    }

    #[test]
    fn backup_path_appends_suffix() {
        assert_eq!(
            backup_path(Path::new("/home/u/.config/mox/config")),
            PathBuf::from("/home/u/.config/mox/config.bak")
        );
        assert_eq!(
            backup_path(Path::new("/x/.env")),
            PathBuf::from("/x/.env.bak")
        );
    }
}
