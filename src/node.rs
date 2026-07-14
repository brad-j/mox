//! The node transport. Talks to Proxmox by running `pvesh`/`qm` over SSH and
//! parsing their JSON output — reusing the automation key `pve` already set up.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::model::{GuestIps, Resource, Vm};

/// The exact `create` remote script from the `pve` bash tool, embedded verbatim
/// and run via `bash -s -- <positional args>` so behavior stays at parity.
/// Positional args (1-indexed): template_id, vmid, name, cores, memory, disk,
/// ip, gateway, nameserver, ci_user, full, start, tags, key_base64, tailscale,
/// authkey_base64, tailnet_domain.
const CREATE_SCRIPT: &str = include_str!("create.sh");

/// The `template` remote script from the `pve` bash tool, embedded verbatim.
/// Positional args: template_id, template_name, image_url, storage, bridge,
/// cpu_type, ci_user, packages, ts_install_url.
const TEMPLATE_SCRIPT: &str = include_str!("template.sh");

/// Tailscale's official installer (adds its apt repo + key). Baked into the
/// template because tailscale isn't in Ubuntu's default repos.
const TAILSCALE_INSTALL_URL: &str = "https://tailscale.com/install.sh";

/// Mandatory package baked into every template (needed for IP reporting + clean
/// shutdowns). Tailscale is installed separately via its install script.
const TEMPLATE_PACKAGE_FLOOR: &str = "qemu-guest-agent";

/// Node identity + tooling, for `mox doctor`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeInfo {
    pub hostname: String,
    pub pve_version: String,
    /// `virt-customize` version string, or `None` if it isn't installed.
    pub virt_customize: Option<String>,
}

/// Everything needed to clone + configure a new VM. Mirrors the option surface
/// of `pve create NAME [options]`.
#[derive(Debug, Clone)]
pub struct CreateSpec {
    pub name: String,
    /// Explicit VM id, or `None` to let the node assign the next free one.
    pub vmid: Option<u32>,
    pub cores: u32,
    pub memory: u32,
    pub disk: u32,
    /// `dhcp` or a static CIDR like `192.168.1.50/24`.
    pub ip: String,
    pub gateway: Option<String>,
    pub nameserver: Option<String>,
    pub ci_user: String,
    /// The SSH *public* key text installed into the VM's cloud-init user.
    pub ssh_pubkey: String,
    pub full_clone: bool,
    pub start: bool,
    pub tags: Option<String>,
    pub tailscale: bool,
}

pub struct Node {
    cfg: Config,
}

impl Node {
    pub fn new(cfg: Config) -> Self {
        Node { cfg }
    }

    pub fn host(&self) -> &str {
        &self.cfg.host
    }

    fn ssh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            self.cfg.port.to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=8".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
        ];
        if let Some(id) = &self.cfg.identity_file {
            if id.is_file() {
                args.push("-i".to_string());
                args.push(id.display().to_string());
                args.push("-o".to_string());
                args.push("IdentitiesOnly=yes".to_string());
            }
        }
        args.push(format!("{}@{}", self.cfg.user, self.cfg.host));
        args
    }

    /// Run a remote command and return its stdout.
    pub fn run(&self, remote: &str) -> Result<String> {
        let out = Command::new("ssh")
            .args(self.ssh_args())
            .arg(remote)
            .output()
            .context("failed to spawn ssh")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "remote command failed: {}",
                stderr.trim().lines().next().unwrap_or("(no stderr)")
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Fetch all QEMU VMs (and templates) with live status/cpu/mem.
    pub fn vms(&self) -> Result<Vec<Vm>> {
        let json = self.run("pvesh get /cluster/resources --type vm --output-format json")?;
        let resources: Vec<Resource> =
            serde_json::from_str(&json).context("parsing pvesh resources JSON")?;
        let mut vms: Vec<Vm> = resources
            .into_iter()
            .filter(|r| r.rtype.as_deref() == Some("qemu"))
            .map(Vm::from)
            .collect();
        vms.sort_by_key(|v| v.vmid);
        Ok(vms)
    }

    /// Ask the guest agent for the VM's LAN and Tailscale IPv4 addresses. Only
    /// works on a running VM with the qemu-guest-agent up; returns an error
    /// otherwise (the agent command exits non-zero), which callers surface as
    /// "unavailable" rather than a fatal error.
    pub fn guest_ips(&self, vmid: u32) -> Result<GuestIps> {
        let json = self.run(&format!("qm guest cmd {vmid} network-get-interfaces"))?;
        GuestIps::from_interfaces_json(&json).context("parsing guest network interfaces")
    }

    /// Boot a VM (`qm start`).
    pub fn start_vm(&self, vmid: u32) -> Result<()> {
        self.run(&format!("qm start {vmid}")).map(|_| ())
    }

    /// Shut a VM down gracefully (up to 60s), falling back to a hard stop —
    /// mirrors the `pve` bash tool's `qm shutdown … --timeout 60 || qm stop`.
    pub fn stop_vm(&self, vmid: u32) -> Result<()> {
        self.run(&format!("qm shutdown {vmid} --timeout 60 || qm stop {vmid}"))
            .map(|_| ())
    }

    /// Stop (best-effort) then delete a VM and purge it from all configs —
    /// mirrors the bash tool's `qm stop … || true; qm destroy … --purge 1`.
    pub fn destroy_vm(&self, vmid: u32) -> Result<()> {
        self.run(&format!(
            "qm stop {vmid} >/dev/null 2>&1 || true; qm destroy {vmid} --purge 1"
        ))
        .map(|_| ())
    }

    /// Node identity + tooling for `doctor`: hostname, Proxmox version, and
    /// whether `virt-customize` is present — gathered in one SSH round trip.
    pub fn node_info(&self) -> Result<NodeInfo> {
        let out = self.run(
            "echo \"HOST=$(hostname)\"; \
             echo \"PVE=$(pveversion 2>/dev/null | head -1)\"; \
             if command -v virt-customize >/dev/null 2>&1; then \
               echo \"VC=$(virt-customize --version 2>/dev/null | head -1)\"; \
             else echo \"VC=\"; fi",
        )?;
        Ok(parse_node_info(&out))
    }

    /// The next free VM id the cluster would assign (`pvesh get /cluster/nextid`),
    /// used to hint the create form.
    pub fn next_id(&self) -> Result<u32> {
        let out = self.run("pvesh get /cluster/nextid")?;
        let trimmed = out.trim().trim_matches('"');
        trimmed
            .parse::<u32>()
            .with_context(|| format!("parsing next id from {trimmed:?}"))
    }

    /// Storage names on the node (`pvesm status`, first column, header skipped).
    pub fn storages(&self) -> Result<Vec<String>> {
        let out = self.run("pvesm status")?;
        Ok(first_column_after_header(&out))
    }

    /// Linux bridge names (`ip -brief link show type bridge`).
    pub fn bridges(&self) -> Result<Vec<String>> {
        let out = self.run("ip -brief link show type bridge")?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(|s| s.to_string())
            .collect())
    }

    /// Run a script over SSH via `bash -s -- <args>`: the script is piped to the
    /// remote shell's stdin and each arg is shell-quoted, so untrusted values
    /// (the VM name, tags) can't break out. Mirrors the bash tool's
    /// `remote_bash`. Returns stdout on success.
    fn run_script(&self, script: &str, args: &[String]) -> Result<String> {
        let mut remote = String::from("bash -s --");
        for a in args {
            remote.push(' ');
            remote.push_str(&shell_quote(a));
        }
        let mut child = Command::new("ssh")
            .args(self.ssh_args())
            .arg(&remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn ssh")?;
        {
            let mut stdin = child.stdin.take().context("ssh stdin unavailable")?;
            stdin
                .write_all(script.as_bytes())
                .context("writing script to ssh stdin")?;
        } // drop stdin → EOF so `bash -s` runs
        let out = child.wait_with_output().context("waiting on ssh")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "remote script failed: {}",
                stderr.trim().lines().last().unwrap_or("(no stderr)")
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Clone + configure a new VM per `spec`, returning its VM id. Runs the
    /// embedded `create.sh` (verbatim from the bash tool) with base64-encoded
    /// key material, exactly as `pve create` does.
    pub fn create(&self, spec: &CreateSpec) -> Result<u32> {
        // base64 the *raw* pubkey text (incl. its trailing newline), matching
        // `base64 < "$ssh_key"`.
        let key_b64 = base64_encode(spec.ssh_pubkey.as_bytes());
        let authkey_b64 = if spec.tailscale {
            let key = self.cfg.tailscale_authkey.as_deref().ok_or_else(|| {
                anyhow!("Tailscale requested but PVE_TAILSCALE_AUTHKEY is not set in the config")
            })?;
            // `base64 <<<"$KEY"` here-strings a trailing newline; replicate it.
            base64_encode(format!("{key}\n").as_bytes())
        } else {
            String::new()
        };
        let args = create_args(spec, &self.cfg, &key_b64, &authkey_b64);
        let out = self.run_script(CREATE_SCRIPT, &args)?;
        parse_vm_id(&out).ok_or_else(|| anyhow!("create finished but returned no VM_ID:\n{out}"))
    }

    /// Build the base template on the node (download cloud image →
    /// `virt-customize` → `qm create`/import → convert to template). Runs the
    /// embedded `template.sh` verbatim, **streaming** its output to the terminal
    /// since it's long-running (multi-GB download + apt). Idempotent: the script
    /// exits early if the template already exists.
    pub fn create_template(&self) -> Result<()> {
        let packages = template_package_list(&self.cfg.template_packages);
        let args = vec![
            self.cfg.template_id.to_string(),
            self.cfg.template_name.clone(),
            self.cfg.image_url.clone(),
            self.cfg.storage.clone(),
            self.cfg.bridge.clone(),
            self.cfg.cpu_type.clone(),
            self.cfg.ci_user.clone(),
            packages,
            TAILSCALE_INSTALL_URL.to_string(),
        ];
        self.run_script_streamed(TEMPLATE_SCRIPT, &args)
    }

    /// Like `run_script`, but inherits the terminal's stdout/stderr so a
    /// long-running script's progress streams live (stdin still carries the
    /// script). Returns only success/failure.
    fn run_script_streamed(&self, script: &str, args: &[String]) -> Result<()> {
        let mut remote = String::from("bash -s --");
        for a in args {
            remote.push(' ');
            remote.push_str(&shell_quote(a));
        }
        let mut child = Command::new("ssh")
            .args(self.ssh_args())
            .arg(&remote)
            .stdin(Stdio::piped())
            .spawn()
            .context("failed to spawn ssh")?;
        {
            let mut stdin = child.stdin.take().context("ssh stdin unavailable")?;
            stdin
                .write_all(script.as_bytes())
                .context("writing script to ssh stdin")?;
        }
        let status = child.wait().context("waiting on ssh")?;
        if !status.success() {
            return Err(anyhow!("template build failed (ssh exit {status})"));
        }
        Ok(())
    }

    /// Write a `~/.ssh/config` entry for a newly-created Tailscale VM so
    /// `ssh <name>` just works, returning the MagicDNS alias it wrote. A no-op
    /// (returns `None`) unless a tailnet domain and an identity file are both
    /// configured. Mirrors the bash tool's `add_ssh_config_vm`.
    pub fn write_vm_ssh_config(&self, name: &str) -> Option<String> {
        let domain = self.cfg.tailnet_domain.as_ref()?;
        let identity = self.cfg.identity_file.as_ref()?;
        let path = crate::sshconfig::default_path()?;
        let alias = format!("{name}.{domain}");
        match crate::sshconfig::upsert_vm_entry(&path, name, &alias, &self.cfg.ci_user, identity) {
            Ok(()) => Some(alias),
            Err(_) => None,
        }
    }

    /// Poll the guest agent until the VM reports a LAN IP (and a Tailscale IP,
    /// when `want_tailscale`), or `timeout` elapses. Returns whatever resolved
    /// (possibly partial). Mirrors `wait_for_guest_ip` / `wait_for_tailscale_ip`.
    pub fn wait_for_ips(&self, vmid: u32, want_tailscale: bool, timeout: Duration) -> GuestIps {
        let deadline = Instant::now() + timeout;
        loop {
            let ips = self.guest_ips(vmid).unwrap_or_default();
            let have_lan = ips.lan.is_some();
            let have_ts = !want_tailscale || ips.tailscale.is_some();
            if (have_lan && have_ts) || Instant::now() >= deadline {
                return ips;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}

/// The comma-separated package list baked into the template: the mandatory
/// floor plus any user extras (from `PVE_TEMPLATE_PACKAGES`, split on whitespace
/// or commas). Mirrors the bash tool's `template_package_list`.
fn template_package_list(extra: &str) -> String {
    let parts: Vec<&str> = extra
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        TEMPLATE_PACKAGE_FLOOR.to_string()
    } else {
        format!("{TEMPLATE_PACKAGE_FLOOR},{}", parts.join(","))
    }
}

/// Assemble the 17 positional args `create.sh` expects, in order.
fn create_args(spec: &CreateSpec, cfg: &Config, key_b64: &str, authkey_b64: &str) -> Vec<String> {
    let flag = |b: bool| if b { "1" } else { "0" }.to_string();
    vec![
        cfg.template_id.to_string(),
        spec.vmid.map(|v| v.to_string()).unwrap_or_default(),
        spec.name.clone(),
        spec.cores.to_string(),
        spec.memory.to_string(),
        spec.disk.to_string(),
        spec.ip.clone(),
        spec.gateway.clone().unwrap_or_default(),
        spec.nameserver.clone().unwrap_or_default(),
        spec.ci_user.clone(),
        flag(spec.full_clone),
        flag(spec.start),
        spec.tags.clone().unwrap_or_default(),
        key_b64.to_string(),
        flag(spec.tailscale),
        authkey_b64.to_string(),
        cfg.tailnet_domain.clone().unwrap_or_default(),
    ]
}

/// Pull the `VM_ID=<n>` line from `create.sh`'s summary output.
fn parse_vm_id(output: &str) -> Option<u32> {
    output
        .lines()
        .find_map(|l| l.strip_prefix("VM_ID=").and_then(|v| v.trim().parse().ok()))
}

/// Wrap a value in single quotes for safe inclusion in a remote command,
/// escaping embedded single quotes (`'` → `'\''`).
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Standard base64 encoding (no line wrapping) — a few lines to avoid a crate.
fn base64_encode(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Parse the `HOST=` / `PVE=` / `VC=` lines emitted by `node_info`'s remote
/// command into a `NodeInfo` (empty `VC=` means `virt-customize` is missing).
fn parse_node_info(out: &str) -> NodeInfo {
    let field = |key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let vc = field("VC=");
    NodeInfo {
        hostname: field("HOST="),
        pve_version: field("PVE="),
        virt_customize: if vc.is_empty() { None } else { Some(vc) },
    }
}

/// First whitespace-delimited column of every line after the header row — the
/// shape of `pvesm status` / `qm list` tabular output the bash tool parses with
/// `awk 'NR>1 {print $1}'`.
fn first_column_after_header(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn shell_quote_neutralizes_injection() {
        assert_eq!(shell_quote("web-01"), "'web-01'");
        // A value trying to break out stays a single literal argument.
        assert_eq!(shell_quote("a'; rm -rf /"), "'a'\\''; rm -rf /'");
    }

    #[test]
    fn parse_vm_id_reads_summary() {
        let out = "Cloning...\nVM_ID=142\nVM_NAME=web-01\nSTARTED=1\n";
        assert_eq!(parse_vm_id(out), Some(142));
        assert_eq!(parse_vm_id("no id here"), None);
    }

    #[test]
    fn create_args_positional_order() {
        let cfg = Config {
            host: "h".into(),
            user: "root".into(),
            port: 22,
            identity_file: None,
            template_id: 9000,
            bridge: "vmbr0".into(),
            template_name: "ubuntu-2404-cloudinit".into(),
            image_url: "https://example.com/img.img".into(),
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
            tailnet_domain: Some("example.ts.net".into()),
            ip_timeout: 120,
            vm_subnet: None,
        };
        let spec = CreateSpec {
            name: "web-01".into(),
            vmid: None,
            cores: 4,
            memory: 8192,
            disk: 40,
            ip: "dhcp".into(),
            gateway: None,
            nameserver: None,
            ci_user: "ubuntu".into(),
            ssh_pubkey: "ssh-ed25519 AAAA".into(),
            full_clone: true,
            start: true,
            tags: Some("dev".into()),
            tailscale: false,
        };
        let args = create_args(&spec, &cfg, "KEYB64", "");
        assert_eq!(args.len(), 17);
        assert_eq!(args[0], "9000"); // template_id
        assert_eq!(args[1], ""); // vmid (None → next id on the node)
        assert_eq!(args[2], "web-01");
        assert_eq!(args[10], "1"); // full clone
        assert_eq!(args[11], "1"); // start
        assert_eq!(args[13], "KEYB64");
        assert_eq!(args[14], "0"); // tailscale off
        assert_eq!(args[16], "example.ts.net");
    }

    #[test]
    fn template_packages_floor_and_extras() {
        assert_eq!(template_package_list(""), "qemu-guest-agent");
        assert_eq!(template_package_list("   "), "qemu-guest-agent");
        assert_eq!(
            template_package_list("vim htop"),
            "qemu-guest-agent,vim,htop"
        );
        // Commas, extra spaces, and mixed separators all normalize cleanly.
        assert_eq!(
            template_package_list("vim, htop,,git"),
            "qemu-guest-agent,vim,htop,git"
        );
    }

    #[test]
    fn parses_node_info_present_and_missing() {
        let present = "HOST=think-1\nPVE=pve-manager/9.2.4\nVC=virt-customize 1.52.3\n";
        let info = parse_node_info(present);
        assert_eq!(info.hostname, "think-1");
        assert_eq!(info.pve_version, "pve-manager/9.2.4");
        assert_eq!(info.virt_customize.as_deref(), Some("virt-customize 1.52.3"));

        // Empty VC= → virt-customize not installed.
        let missing = "HOST=n\nPVE=x\nVC=\n";
        assert_eq!(parse_node_info(missing).virt_customize, None);
    }

    #[test]
    fn parses_pvesm_status_names() {
        let sample = "\
Name             Type     Status           Total            Used       Available        %
local            dir      active        98559220        12897012        80612624   13.08%
local-lvm        lvmthin  active       142606336        23074406       119531929   16.18%";
        assert_eq!(
            first_column_after_header(sample),
            vec!["local".to_string(), "local-lvm".to_string()]
        );
    }

    #[test]
    fn empty_or_header_only_yields_nothing() {
        assert!(first_column_after_header("").is_empty());
        assert!(first_column_after_header("Name Type Status").is_empty());
    }
}
