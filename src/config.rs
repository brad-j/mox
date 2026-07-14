//! Configuration loading. For now we reuse the existing `pve` `.env` file so
//! `mox` works with zero new setup; a native config format can come later.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

/// Connection settings for reaching the Proxmox node over SSH, plus the
/// create-time defaults the bash tool keeps in the same `.env`.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity_file: Option<PathBuf>,
    // Create defaults (mirrors the `PVE_*` keys `pve create` reads).
    pub template_id: u32,
    pub bridge: String,
    // Template-build inputs (`mox template`).
    pub template_name: String,
    pub image_url: String,
    pub storage: String,
    pub cpu_type: String,
    pub template_packages: String,
    pub default_cores: u32,
    pub default_memory: u32,
    pub default_disk: u32,
    pub default_full_clone: bool,
    pub ci_user: String,
    pub tailscale_default: bool,
    pub tailscale_authkey: Option<String>,
    pub tailnet_domain: Option<String>,
    pub ip_timeout: u64,
    /// VM subnet (e.g. `192.168.3.0/24`) — used by `setup-access` to wire the
    /// `~/.ssh/config` subnet block so `ssh <lan-ip>` uses the automation key.
    pub vm_subnet: Option<String>,
}

impl Config {
    /// Load config from the first file found (via `PVE_CONFIG_FILE`, then
    /// walking up from the cwd, then `~/.config/mox/config`, then
    /// `~/.config/pve/.env`).
    pub fn load() -> Result<Config> {
        let path = find_env_file()
            .ok_or_else(|| anyhow!("no config found — run `mox init` or set PVE_CONFIG_FILE"))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse_env(&text).map_err(|e| anyhow!("{e} (in {})", path.display()))
    }

    /// Built-in defaults with an empty host (which `parse_env` requires be set).
    pub fn defaults() -> Config {
        Config {
            host: String::new(),
            user: "root".into(),
            port: 22,
            identity_file: None,
            template_id: 9000,
            bridge: "vmbr0".into(),
            template_name: "ubuntu-2404-cloudinit".into(),
            image_url:
                "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
                    .into(),
            storage: "local-lvm".into(),
            cpu_type: "host".into(),
            template_packages: String::new(),
            default_cores: 2,
            default_memory: 4096,
            default_disk: 20,
            default_full_clone: false,
            ci_user: "ubuntu".into(),
            tailscale_default: false,
            tailscale_authkey: None,
            tailnet_domain: None,
            ip_timeout: 120,
            vm_subnet: None,
        }
    }

    /// Parse `.env`-style `KEY=value` text over the built-in defaults. Errors if
    /// `PVE_HOST` isn't set.
    fn parse_env(text: &str) -> Result<Config> {
        let mut cfg = Config::defaults();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let value = unquote(strip_inline_comment(v.trim()));
            match k.trim() {
                "PVE_HOST" if !value.is_empty() => cfg.host = value,
                "PVE_USER" if !value.is_empty() => cfg.user = value,
                "PVE_SSH_PORT" => {
                    if let Ok(p) = value.parse() {
                        cfg.port = p;
                    }
                }
                "PVE_IDENTITY_FILE" if !value.is_empty() => {
                    cfg.identity_file = Some(PathBuf::from(value))
                }
                "PVE_TEMPLATE_ID" => {
                    if let Ok(n) = value.parse() {
                        cfg.template_id = n;
                    }
                }
                "PVE_BRIDGE" if !value.is_empty() => cfg.bridge = value,
                "PVE_TEMPLATE_NAME" if !value.is_empty() => cfg.template_name = value,
                "PVE_IMAGE_URL" if !value.is_empty() => cfg.image_url = value,
                "PVE_STORAGE" if !value.is_empty() => cfg.storage = value,
                "PVE_CPU_TYPE" if !value.is_empty() => cfg.cpu_type = value,
                "PVE_TEMPLATE_PACKAGES" => cfg.template_packages = value,
                "PVE_DEFAULT_CORES" => {
                    if let Ok(n) = value.parse() {
                        cfg.default_cores = n;
                    }
                }
                "PVE_DEFAULT_MEMORY" => {
                    if let Ok(n) = value.parse() {
                        cfg.default_memory = n;
                    }
                }
                "PVE_DEFAULT_DISK" => {
                    if let Ok(n) = value.parse() {
                        cfg.default_disk = n;
                    }
                }
                "PVE_DEFAULT_FULL_CLONE" => cfg.default_full_clone = value == "1",
                "PVE_CI_USER" if !value.is_empty() => cfg.ci_user = value,
                "PVE_TAILSCALE_DEFAULT" => cfg.tailscale_default = value == "1",
                "PVE_TAILSCALE_AUTHKEY" if !value.is_empty() => {
                    cfg.tailscale_authkey = Some(value)
                }
                "PVE_TAILNET_DOMAIN" if !value.is_empty() => cfg.tailnet_domain = Some(value),
                "PVE_IP_TIMEOUT" => {
                    if let Ok(n) = value.parse() {
                        cfg.ip_timeout = n;
                    }
                }
                "PVE_VM_SUBNET" if !value.is_empty() => cfg.vm_subnet = Some(value),
                _ => {}
            }
        }
        if cfg.host.is_empty() {
            return Err(anyhow!("PVE_HOST is not set"));
        }
        Ok(cfg)
    }

    /// Serialize back to `.env`-style text (what `mox init` writes). Round-trips
    /// through `parse_env`.
    pub fn to_env_string(&self) -> String {
        let id = self
            .identity_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        format!(
            "# Generated by `mox init` on {host}. Re-run `mox init` to regenerate.\n\
             PVE_HOST={host}\n\
             PVE_USER={user}\n\
             PVE_SSH_PORT={port}\n\
             PVE_IDENTITY_FILE=\"{id}\"\n\n\
             PVE_STORAGE={storage}\n\
             PVE_BRIDGE={bridge}\n\n\
             PVE_TEMPLATE_ID={tid}\n\
             PVE_TEMPLATE_NAME={tname}\n\
             PVE_IMAGE_URL={img}\n\
             PVE_CPU_TYPE={cpu}\n\
             PVE_CI_USER={ci}\n\n\
             PVE_DEFAULT_CORES={dc}\n\
             PVE_DEFAULT_MEMORY={dm}\n\
             PVE_DEFAULT_DISK={dd}\n\
             PVE_DEFAULT_FULL_CLONE={fc}\n\n\
             # Extra packages baked into the template (comma-separated); the\n\
             # qemu-guest-agent + tailscale floor is always installed.\n\
             PVE_TEMPLATE_PACKAGES={pkgs}\n\n\
             # Whether new VMs join the tailnet by default (1) or not (0).\n\
             PVE_TAILSCALE_DEFAULT={tsd}\n\
             PVE_TAILSCALE_AUTHKEY={tsk}\n\
             PVE_TAILNET_DOMAIN={tnd}\n\n\
             # Subnet of DHCP VMs, for the ~/.ssh/config subnet block.\n\
             PVE_VM_SUBNET={subnet}\n\n\
             # Seconds to wait for an IP at create time.\n\
             PVE_IP_TIMEOUT={ipt}\n",
            host = self.host,
            user = self.user,
            port = self.port,
            id = id,
            storage = self.storage,
            bridge = self.bridge,
            tid = self.template_id,
            tname = self.template_name,
            img = self.image_url,
            cpu = self.cpu_type,
            ci = self.ci_user,
            dc = self.default_cores,
            dm = self.default_memory,
            dd = self.default_disk,
            fc = self.default_full_clone as u8,
            pkgs = self.template_packages,
            tsd = self.tailscale_default as u8,
            tsk = self.tailscale_authkey.as_deref().unwrap_or(""),
            tnd = self.tailnet_domain.as_deref().unwrap_or(""),
            subnet = self.vm_subnet.as_deref().unwrap_or(""),
            ipt = self.ip_timeout,
        )
    }
}

/// The native config location `mox init` writes to: `~/.config/mox/config`.
pub fn init_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/mox/config"))
}

/// Drop a trailing ` # comment` from a value. Only splits on a `#` that follows
/// whitespace, so `#` inside a quoted value or a URL fragment is preserved.
fn strip_inline_comment(s: &str) -> &str {
    if let Some(idx) = s.find(" #") {
        s[..idx].trim_end()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_inline_comment, Config};

    #[test]
    fn env_string_round_trips() {
        let mut cfg = Config::defaults();
        cfg.host = "192.0.2.10".into();
        cfg.user = "root".into();
        cfg.port = 2222;
        cfg.identity_file = Some(std::path::PathBuf::from("/home/u/.ssh/proxmox/id_ed25519"));
        cfg.tailscale_default = true;
        cfg.tailscale_authkey = Some("tskey-auth-xxx".into());
        cfg.tailnet_domain = Some("example.ts.net".into());
        cfg.vm_subnet = Some("192.0.2.0/24".into());
        cfg.template_packages = "vim,htop".into();

        let parsed = Config::parse_env(&cfg.to_env_string()).unwrap();
        assert_eq!(parsed.host, "192.0.2.10");
        assert_eq!(parsed.port, 2222);
        assert_eq!(parsed.identity_file, cfg.identity_file);
        assert!(parsed.tailscale_default);
        assert_eq!(parsed.tailscale_authkey.as_deref(), Some("tskey-auth-xxx"));
        assert_eq!(parsed.tailnet_domain.as_deref(), Some("example.ts.net"));
        assert_eq!(parsed.vm_subnet.as_deref(), Some("192.0.2.0/24"));
        assert_eq!(parsed.template_packages, "vim,htop");
    }

    #[test]
    fn parse_env_requires_host() {
        assert!(Config::parse_env("PVE_USER=root\n").is_err());
        assert!(Config::parse_env("PVE_HOST=10.0.0.1\n").is_ok());
    }

    #[test]
    fn strips_trailing_comment_but_keeps_hash_in_value() {
        assert_eq!(strip_inline_comment("192.0.2.10   # node IP"), "192.0.2.10");
        assert_eq!(strip_inline_comment("root"), "root");
        // A '#' not preceded by whitespace (e.g. a URL fragment) is preserved.
        assert_eq!(
            strip_inline_comment("https://example.com/img#frag"),
            "https://example.com/img#frag"
        );
    }
}

/// Strip one layer of surrounding quotes and expand a leading `$HOME` / `~`.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(s);
    expand_home(s)
}

fn expand_home(s: &str) -> String {
    let home = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned());
    if let Some(home) = home {
        if let Some(rest) = s.strip_prefix("$HOME") {
            return format!("{home}{rest}");
        }
        if let Some(rest) = s.strip_prefix('~') {
            return format!("{home}{rest}");
        }
    }
    s.to_string()
}

fn find_env_file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PVE_CONFIG_FILE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let candidate = d.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    // Native location first, then the legacy pve `.env` fallback.
    if let Some(home) = std::env::var_os("HOME") {
        for rel in [".config/mox/config", ".config/pve/.env"] {
            let c = PathBuf::from(&home).join(rel);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}
