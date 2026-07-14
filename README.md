# mox

A terminal UI for managing Proxmox VMs — think "k9s for VMs". Written in Rust.

`mox` reaches a Proxmox node over SSH and drives its native `pvesh`/`qm`
commands (asking for JSON output), so it works against a stock Proxmox install
with no agent or API server setup — just SSH key access to the node.

The name comes from Prox**mox** (and *moxie*).

## Status

Early, but real and runnable:

- `mox` — a live, full-screen dashboard: colored status dots, CPU/mem gauges,
  auto-refresh, keyboard navigation.
- `mox list` — a plain, scriptable table of VMs.

## Build & run

```bash
cargo build
./target/debug/mox          # live dashboard
./target/debug/mox list     # plain table
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
