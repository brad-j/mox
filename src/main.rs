mod config;
mod init;
mod model;
mod node;
mod setup;
mod sshconfig;
mod tui;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use config::Config;
use node::Node;

#[derive(Parser)]
#[command(name = "mox", version, about = "A terminal UI for managing Proxmox VMs")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Interactive setup — writes ~/.config/mox/config
    Init,
    /// List VMs on the node
    List {
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Check connection + node health (read-only)
    Doctor {
        /// Output JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Generate + install the automation SSH key and wire ~/.ssh/config
    SetupAccess,
    /// Build the base cloud-init template on the node (long-running)
    Template,
    /// Print a shell completion script (bash, zsh, fish, elvish, powershell)
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Init) => init::run(),
        Some(Commands::List { json }) => cmd_list(json),
        Some(Commands::Doctor { json }) => cmd_doctor(json),
        Some(Commands::SetupAccess) => {
            let cfg = Config::load()?;
            setup::run(&cfg)
        }
        Some(Commands::Template) => {
            let cfg = Config::load()?;
            println!(
                "Building template {} ({}) on {}…\n",
                cfg.template_name, cfg.template_id, cfg.host
            );
            Node::new(cfg).create_template()
        }
        Some(Commands::Completions { shell }) => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }
        None => {
            let cfg = Config::load()?;
            tui::run(cfg)
        }
    }
}

fn cmd_doctor(json: bool) -> Result<()> {
    let cfg = Config::load()?;
    let node = Node::new(cfg.clone());

    // Probe the node once. Only chase storages/bridges if it's reachable, so an
    // unreachable node fails fast instead of timing out three times.
    let info = node.node_info();
    let (storages, bridges) = if info.is_ok() {
        (node.storages().ok(), node.bridges().ok())
    } else {
        (None, None)
    };

    if json {
        let node_json = match &info {
            Ok(i) => serde_json::json!({
                "reachable": true,
                "hostname": i.hostname,
                "proxmox": i.pve_version,
                "virt_customize": i.virt_customize,
                "storages": storages,
                "bridges": bridges,
            }),
            Err(e) => serde_json::json!({ "reachable": false, "error": e.to_string() }),
        };
        let out = serde_json::json!({
            "connection": {
                "host": cfg.host,
                "web_ui": format!("https://{}:8006", cfg.host),
                "user": cfg.user,
                "port": cfg.port,
                "ssh_key": cfg.identity_file.as_ref().map(|p| p.display().to_string()),
                "template_id": cfg.template_id,
                "tailnet": cfg.tailnet_domain,
                "tailscale_configured": cfg.tailscale_authkey.is_some(),
            },
            "node": node_json,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("mox doctor\n");
    println!("Connection");
    println!("  host        {}", cfg.host);
    println!("  web UI      https://{}:8006", cfg.host);
    println!("  ssh         {}@{}:{}", cfg.user, cfg.host, cfg.port);
    println!(
        "  ssh key     {}",
        cfg.identity_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none configured)".into())
    );
    println!("  template    {}", cfg.template_id);
    println!(
        "  tailnet     {}",
        cfg.tailnet_domain.as_deref().unwrap_or("(unset)")
    );
    println!(
        "  tailscale   {}",
        if cfg.tailscale_authkey.is_some() {
            "configured"
        } else {
            "(no auth key — new VMs won't join the tailnet)"
        }
    );

    println!("\nNode status");
    match info {
        Ok(i) => {
            println!("  hostname        {}", i.hostname);
            println!("  proxmox         {}", i.pve_version);
            println!(
                "  virt-customize  {}",
                i.virt_customize
                    .unwrap_or_else(|| "MISSING (auto-installed by template build)".into())
            );
            println!("  storages        {}", join_or_dash(&storages.unwrap_or_default()));
            println!("  bridges         {}", join_or_dash(&bridges.unwrap_or_default()));
        }
        // A connection failure here is the whole point of doctor — report it.
        Err(e) => println!("  unreachable: {e}"),
    }
    Ok(())
}

fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "—".to_string()
    } else {
        items.join(", ")
    }
}

fn cmd_list(json: bool) -> Result<()> {
    let cfg = Config::load()?;
    let node = Node::new(cfg);
    let vms = node.vms()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&vms)?);
        return Ok(());
    }

    println!("node {}\n", node.host());
    println!(
        "{:<6} {:<20} {:<10} {:>6} {:>6} {:>8}",
        "ID", "NAME", "STATUS", "CPU%", "MEM%", "UPTIME"
    );
    for vm in &vms {
        println!(
            "{:<6} {:<20} {:<10} {:>5.0}% {:>5.0}% {:>8}",
            vm.vmid,
            truncate(&vm.name, 20),
            vm.status,
            vm.cpu_pct(),
            vm.mem_pct(),
            vm.uptime_human()
        );
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}
