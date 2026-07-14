//! Local `~/.ssh/config` management for created VMs. Mirrors the `pve` bash
//! tool's `add_ssh_config_vm` / `remove_ssh_config_vm`: a per-VM `Host` block
//! wrapped in marker comments so it can be idempotently replaced or removed.
//! mox uses its own marker (`# mox-managed`) so it never disturbs pve's blocks.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Comment that brackets a mox-managed block, so we can find and replace it.
const MARKER: &str = "# mox-managed";

/// Default config location: `$HOME/.ssh/config`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ssh/config"))
}

/// Insert (or replace) the `Host` block for `name` pointing at `alias` (its
/// Tailscale MagicDNS hostname). Creates `~/.ssh` (0700) and the file (0600) if
/// needed. Idempotent: re-running replaces the VM's existing block.
pub fn upsert_vm_entry(
    path: &Path,
    name: &str,
    alias: &str,
    user: &str,
    identity: &Path,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        set_mode(parent, 0o700);
    }
    let existing = fs::read_to_string(path).unwrap_or_default();
    let stripped = strip_block(&existing, name);
    let block = render_block(name, alias, user, &identity.display().to_string());
    // One blank line separates the block from any prior content.
    let combined = format!("{}{}", stripped.trim_end_matches('\n'), block);
    fs::write(path, combined).with_context(|| format!("writing {}", path.display()))?;
    set_mode(path, 0o600);
    Ok(())
}

/// Ensure a `# mox-managed subnet` block exists so `ssh <lan-ip>` on the VM
/// subnet uses the automation key + cloud-init user. Idempotent: does nothing if
/// the subnet block is already present. Returns `true` if it wrote one. Mirrors
/// the bash tool's `ensure_ssh_config_block`.
pub fn ensure_subnet_block(
    path: &Path,
    subnet: &str,
    user: &str,
    identity: &Path,
) -> Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        set_mode(parent, 0o700);
    }
    let existing = fs::read_to_string(path).unwrap_or_default();
    let subnet_marker = format!("{MARKER} subnet");
    if existing.lines().any(|l| l == subnet_marker) {
        return Ok(false); // already present
    }
    let pattern = subnet_host_pattern(subnet);
    let block = format!(
        "\n{MARKER} subnet\n\
         Host {pattern}\n    \
         User {user}\n    \
         IdentityFile {}\n    \
         IdentitiesOnly yes\n\
         {MARKER} end\n",
        identity.display()
    );
    let combined = format!("{}{}", existing.trim_end_matches('\n'), block);
    fs::write(path, combined).with_context(|| format!("writing {}", path.display()))?;
    set_mode(path, 0o600);
    Ok(true)
}

/// Turn a subnet like `192.168.3.0/24` into an ssh `Host` glob `192.168.3.*`
/// (first three octets). Falls back to the input if it can't be parsed.
fn subnet_host_pattern(subnet: &str) -> String {
    let addr = subnet.split('/').next().unwrap_or(subnet);
    let octets: Vec<&str> = addr.split('.').collect();
    if octets.len() >= 3 {
        format!("{}.{}.{}.*", octets[0], octets[1], octets[2])
    } else {
        subnet.to_string()
    }
}

/// The block text, including its leading blank line and marker comments.
fn render_block(name: &str, alias: &str, user: &str, identity: &str) -> String {
    format!(
        "\n{MARKER}: {name}\n\
         Host {name} {alias}\n    \
         HostName {alias}\n    \
         User {user}\n    \
         IdentityFile {identity}\n    \
         IdentitiesOnly yes\n\
         {MARKER} end\n"
    )
}

/// Remove any existing mox-managed block for `name` — everything from the
/// `# mox-managed: <name>` line through the next `# mox-managed end`, inclusive.
/// Ports the bash tool's awk block filter.
fn strip_block(content: &str, name: &str) -> String {
    let marker = format!("{MARKER}: {name}");
    let end = format!("{MARKER} end");
    let mut out = String::new();
    let mut skipping = false;
    for line in content.lines() {
        if !skipping && line == marker {
            skipping = true;
            continue;
        }
        if skipping {
            if line == end {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
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
    fn insert_then_idempotent_replace() {
        let dir = std::env::temp_dir().join("mox-sshconfig-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config");
        let _ = fs::remove_file(&path);

        let id = PathBuf::from("/home/u/.ssh/proxmox/id_ed25519");
        upsert_vm_entry(&path, "web-01", "web-01.example.ts.net", "ubuntu", &id).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.contains("Host web-01 web-01.example.ts.net"));
        assert!(first.contains("User ubuntu"));
        assert_eq!(first.matches("# mox-managed: web-01").count(), 1);

        // Re-running replaces (not duplicates) the block.
        upsert_vm_entry(&path, "web-01", "web-01.example.ts.net", "root", &id).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(second.matches("# mox-managed: web-01").count(), 1);
        assert!(second.contains("User root"));
        assert!(!second.contains("User ubuntu"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn subnet_pattern_from_cidr() {
        assert_eq!(subnet_host_pattern("192.168.3.0/24"), "192.168.3.*");
        assert_eq!(subnet_host_pattern("10.0.0.0/8"), "10.0.0.*");
        assert_eq!(subnet_host_pattern("weird"), "weird");
    }

    #[test]
    fn subnet_block_is_idempotent() {
        let dir = std::env::temp_dir().join("mox-subnet-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config");
        let _ = fs::remove_file(&path);
        let id = PathBuf::from("/home/u/.ssh/proxmox/id_ed25519");

        let wrote1 = ensure_subnet_block(&path, "192.168.3.0/24", "ubuntu", &id).unwrap();
        assert!(wrote1, "first call writes the block");
        let wrote2 = ensure_subnet_block(&path, "192.168.3.0/24", "ubuntu", &id).unwrap();
        assert!(!wrote2, "second call is a no-op");

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("# mox-managed subnet").count(), 1);
        assert!(content.contains("Host 192.168.3.*"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn strip_leaves_unmanaged_content_intact() {
        let existing = "Host bastion\n    HostName 10.0.0.9\n\n# mox-managed: old\nHost old x\n# mox-managed end\n";
        let stripped = strip_block(existing, "old");
        assert!(stripped.contains("Host bastion"));
        assert!(!stripped.contains("Host old x"));
        assert!(!stripped.contains("# mox-managed"));
    }
}
