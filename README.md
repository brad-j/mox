# mox

A terminal UI for managing Proxmox VMs — think "k9s for VMs". Written in Rust.

`mox` reaches a Proxmox node over SSH and drives its native `pvesh`/`qm`
commands (asking for JSON output), so it works against a stock Proxmox install
with no agent or API server setup — just SSH key access to the node.

The name comes from Prox**mox** (and *moxie*).

> ⚠️ **Heads up: this project is 100% vibe coded.** It was built almost
> entirely by prompting an AI coding assistant, with light human steering. It
> works for me, but it hasn't been battle-tested, professionally audited, or
> hardened. It runs `qm`/`pvesh` commands against your Proxmox node over SSH —
> including destructive ones like `destroy`. **Use at your own risk**, ideally
> against a node you can afford to break first. No warranty; see [LICENSE](LICENSE).

## Status

Early, but real and runnable:

- `mox` — a live, full-screen dashboard: colored status dots, CPU/mem gauges,
  auto-refresh, keyboard navigation.
- `mox list` — a plain, scriptable table of VMs.

## Install

Download a prebuilt binary from the
[latest release](https://github.com/brad-j/mox/releases/latest) — Linux and
macOS, x86_64 and arm64. No Rust toolchain required.

```bash
# example: macOS on Apple Silicon (adjust tag + target for your platform)
curl -LO https://github.com/brad-j/mox/releases/latest/download/mox-v0.1.0-aarch64-apple-darwin.tar.gz
tar xzf mox-v0.1.0-aarch64-apple-darwin.tar.gz
sudo mv mox /usr/local/bin/
mox --version
```

Available targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. Each archive ships with a
`.sha256` checksum.

## Build from source

```bash
cargo build --release
./target/release/mox        # live dashboard
./target/release/mox list   # plain table
```

### Dashboard keys

| Key | Action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | move selection |
| `r` | refresh now |
| `q` / `Esc` | quit |

The dashboard auto-refreshes every 3 seconds.

## Configuration

`mox` reads a `.env` file (the same schema as the sibling `pve` bash tool).
See [`.env.example`](.env.example). Discovery order:

1. `$PVE_CONFIG_FILE` (explicit path)
2. the nearest `.env` walking up from the current directory
3. `~/.config/pve/.env`

```bash
# quick start against an existing pve config
PVE_CONFIG_FILE=~/Code/proxmox-vm-helper/.env mox
```

> **Note:** standalone config discovery is the current top priority — see
> `CLAUDE.md`. Until then, run from a directory with a `.env` or set
> `PVE_CONFIG_FILE`.

## Relationship to `pve`

`mox` was extracted from the [`proxmox-vm-helper`](../proxmox-vm-helper) project,
a Bash tool (`pve`) that provisions Ubuntu cloud-init VMs on Proxmox. `pve` still
owns provisioning (template creation, cloud-init, Tailscale). `mox` currently
reuses `pve`'s `.env` and focuses on live management/observability. Whether `mox`
eventually absorbs provisioning is an open question (see `CLAUDE.md`).

## License

MIT
