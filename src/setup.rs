//! `mox setup-access` — one-time host bootstrap: generate the dedicated
//! automation SSH key, install its public key into the node (one password
//! prompt), and wire the `~/.ssh/config` subnet block. Ports the bash tool's
//! `setup_access`. The key-install step is interactive, so this is a CLI verb,
//! not a TUI action.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::Config;
use crate::node::Node;
use crate::sshconfig;

pub fn run(cfg: &Config) -> Result<()> {
    let identity = cfg
        .identity_file
        .as_ref()
        .ok_or_else(|| anyhow!("no SSH identity configured (set PVE_IDENTITY_FILE)"))?;

    // 1. Generate the dedicated key if it's not there yet.
    if identity.exists() {
        println!("Using existing SSH key: {}", identity.display());
    } else {
        if let Some(dir) = identity.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            set_mode(dir, 0o700);
        }
        println!("Creating dedicated Proxmox SSH key: {}", identity.display());
        run_interactive(
            Command::new("ssh-keygen")
                .arg("-t")
                .arg("ed25519")
                .arg("-f")
                .arg(identity)
                .arg("-N")
                .arg("")
                .arg("-C")
                .arg("mox-automation"),
        )?;
    }

    // 2. Install the public key into the node — prompts once for the password.
    let target = format!("{}@{}", cfg.user, cfg.host);
    println!(
        "\nYou should be prompted once for the {} password on {}.",
        cfg.user, cfg.host
    );
    let pubkey = format!("{}.pub", identity.display());
    run_interactive(
        Command::new("ssh-copy-id")
            .arg("-i")
            .arg(&pubkey)
            .arg("-p")
            .arg(cfg.port.to_string())
            .arg(&target),
    )?;

    // 3. Wire the local ~/.ssh/config subnet block so `ssh <lan-ip>` just works.
    println!("\nWiring local ssh config...");
    match (&cfg.vm_subnet, sshconfig::default_path()) {
        (Some(subnet), Some(path)) => {
            if sshconfig::ensure_subnet_block(&path, subnet, &cfg.ci_user, identity)? {
                println!("Added subnet block to {}", path.display());
            } else {
                println!("ssh config subnet block already present.");
            }
        }
        (None, _) => println!("(no PVE_VM_SUBNET set — skipping ~/.ssh/config subnet block)"),
        (_, None) => println!("(no HOME — skipping ~/.ssh/config subnet block)"),
    }

    // 4. Verify access over the (now installed) key.
    println!("\nChecking access...");
    let node = Node::new(cfg.clone());
    match node.node_info() {
        Ok(info) => println!("Connected to {} ({}).", info.hostname, info.pve_version),
        Err(e) => bail!("access check failed: {e}"),
    }
    println!("\nSetup complete. Try: mox doctor");
    Ok(())
}

/// Run a command inheriting the terminal (so interactive prompts work), with a
/// friendly error if the binary isn't installed.
fn run_interactive(cmd: &mut Command) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd.status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("`{program}` not found — is it installed and on your PATH?")
        } else {
            anyhow!("failed to run `{program}`: {e}")
        }
    })?;
    if !status.success() {
        bail!("`{program}` exited with {status}");
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}
