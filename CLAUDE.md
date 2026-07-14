# mox — project context & handoff

`mox` is a Rust terminal UI for managing Proxmox VMs ("k9s for VMs"). This file
is the working context for continuing development. Read it first.

## Origin / ancestry

`mox` was built inside, then extracted from, the sibling Bash project at
`~/Code/proxmox-vm-helper` — a tool (`pve`) that provisions Ubuntu cloud-init
VMs on a Proxmox node over SSH (driving native `qm`/`pvesh`, no Terraform). Key
facts inherited from that project:

- The node is reached over SSH using a dedicated automation key
  (`~/.ssh/proxmox/id_ed25519`), which is also installed into every VM.
- Connection settings live in a `.env` file (schema below). `mox` reuses it.
- A typical node (e.g. `192.0.2.10`) hosts VMs like `web-01` (100) and `db-01`
  (101). A template `ubuntu-2404-cloudinit` (9000) is cloned for new VMs.
  Tailscale membership is optional per VM.
- History note: `mox` was moved out of the git repo as plain files with no git
  history (deliberate). The user creates the new git repo. The prior Rust
  commits ("foundation", "live TUI dashboard") are gone by design — this file is
  the record of what they contained.

## Design decisions

- **Transport = SSH + `pvesh`/`qm --output-format json`.** Reuses the existing
  automation key, needs zero API-token setup, and still yields structured JSON
  plus live cpu/mem via `pvesh get /cluster/resources`. A direct HTTPS
  API-token transport is a possible future drop-in (would enable task-progress
  polling and metrics), but SSH is the deliberate starting point.
- **Config = reuse `pve`'s `.env`** so it works with zero new setup. See the
  "Top priority" task about making this standalone.
- **Lean deps:** `clap` (CLI), `ratatui` + `crossterm` (TUI), `serde`/
  `serde_json`, `anyhow`. No `tokio`/`reqwest` — background refresh uses a
  std thread + `mpsc` channel, which keeps the tree small and compilation fast.
- **Name:** `mox` (from Prox**mox** / *moxie*). Easy to change later via a
  crate/binary rename if it doesn't stick.

## Architecture

```
src/
  main.rs     clap CLI; `list` prints a table, no subcommand launches the TUI
  config.rs   Config::load() — finds & parses .env (quotes + $HOME/~ expansion)
  model.rs    Resource (raw pvesh JSON) -> Vm (cleaned view + formatting helpers)
  node.rs     Node — builds ssh args, runs remote commands, Node::vms()
  tui.rs      the dashboard: background fetcher thread, App state, ratatui render
```

Data flow for the dashboard: a background thread calls `Node::vms()` on a 3s
interval (or on demand via a request channel) and sends `Update`s over an `mpsc`
channel; the UI thread owns `App`, drains updates non-blockingly each loop,
renders, and handles `crossterm` key events.

### `.env` schema (read by config.rs)

| Key | Default | Notes |
|-----|---------|-------|
| `PVE_HOST` | — (required) | node IP or hostname |
| `PVE_USER` | `root` | SSH user |
| `PVE_SSH_PORT` | `22` | SSH port |
| `PVE_IDENTITY_FILE` | none | SSH key; `-i` added only if the file exists |

Config discovery order: `$PVE_CONFIG_FILE` → nearest `.env` walking up from cwd
→ `~/.config/pve/.env`.

## What works today

- `mox init` — interactive setup wizard → writes `~/.config/mox/config`
  (native config location, first in the discovery order).
- `mox list [--json]` — SSH → `pvesh /cluster/resources` → parsed `Vm`s → table
  of id/name/status/cpu%/mem%/uptime (or a JSON array with `--json`).
- `mox doctor [--json]` — read-only preflight: connection summary (from config) +
  node hostname/Proxmox version/`virt-customize`/storages/bridges (live over
  SSH); `--json` emits a `{connection, node}` object.
- `mox setup-access` — one-time bootstrap: `ssh-keygen` the automation key,
  `ssh-copy-id` it to the node, wire the `~/.ssh/config` subnet block, verify.
- `mox template` — build the base cloud-init template on the node (streams
  progress; idempotent). Full `pve` command parity reached with this verb.
- `mox completions <shell>` — print a shell completion script (bash/zsh/fish/
  elvish/powershell).
- `mox` — live dashboard: header (node, VM count, refresh state), bordered VM
  table with colored status dots (`●` running, `◆` template) and `████░░░░`
  CPU/mem gauges colored by load, `▶` selection cursor, footer keybindings.
  Keys: `j`/`k`/`↑`/`↓` move, `Enter` detail overlay, `s`/`x`/`d`
  start/stop/destroy (with confirm modal), `c` create wizard, `r` refresh,
  `q`/`Esc` quit (`Esc` closes an open overlay/modal first).
- **Detail overlay** (roadmap #2, done) — `Enter` on a VM opens a centered popup
  with vCPU/memory/disk/uptime and, resolved asynchronously via the guest agent
  (`qm guest cmd VMID network-get-interfaces`), its LAN + Tailscale IPv4s. The
  IP lookup runs on the background fetcher thread (new `Req::Ips`/`Update::Ips`
  messages) so the UI never blocks; Tailscale vs LAN is split by the CGNAT range
  `100.64.0.0/10` (`model::GuestIps`). Non-running VMs show "VM not running";
  agent failures show a short reason instead of erroring out.
- **Lifecycle actions** (roadmap #3, done) — `s`tart / `x`stop / `d`estroy on the
  selected VM open a centered confirmation modal (`render_confirm`); `y`/`n`
  confirm/cancel. Destroy is red-bordered with an "irreversible" warning; on a
  template only destroy is offered (`begin_action` guard). Confirming dispatches
  `Req::Action` to the fetcher thread, which runs the `qm` command, sends
  `Update::ActionDone`, then immediately re-polls so status reflects live; the
  modal shows a "starting…/stopping…/destroying…" progress line while it runs and
  failures land in the footer. Commands are ported verbatim from the bash tool:
  start=`qm start`, stop=`qm shutdown --timeout 60 || qm stop`, destroy=`qm stop
  … || true; qm destroy … --purge 1` (see `node.rs`). NOTE: a graceful stop
  blocks the fetcher thread up to 60s (no live cpu/mem during that window) — a
  per-action thread is the future fix if it bites.
- **Create wizard** (roadmap #4b/#4c, done) — `c` opens a centered form
  (`CreateForm`) seeded from the config defaults: name, id, cores, memory, disk,
  ip (dhcp/CIDR), gateway, nameserver, user, tags, and full-clone / start /
  tailscale toggles. `↑`/`↓` move, typing edits (digits-only on numeric fields,
  `←`/`→`/`space` toggle bools), `Enter` validates + submits, `Esc` cancels. On
  submit the form reads the local pubkey (`<identity_file>.pub`), builds a
  `CreateSpec`, and dispatches `Req::Create`; the fetcher runs `Node::create`,
  refreshes, waits for IPs, writes the `~/.ssh/config` alias for a Tailscale VM,
  and returns a `CreateOutcome` shown on the form's final screen (with
  ready-to-paste `ssh user@ip` and, for Tailscale VMs, `ssh <name>`). Overlay
  precedence: form > confirm > detail.
- Config now parses the full `PVE_*` create-defaults schema (template id,
  bridge, default cores/mem/disk, full-clone, ci user, tailscale default/authkey/
  tailnet, ip timeout), with inline-comment stripping so `.env.example` parses.
- `TestBackend` render tests in `tui.rs` (dashboard, detail, destroy-confirm,
  create-form) plus `model.rs`/`node.rs`/`config.rs` unit tests (guest-IP
  classification, base64 vectors, shell-quote injection, arg order, comment
  stripping) — 17 total — verify behavior headlessly.

## Direction: full native replacement (decided)

`mox` will become a **full native-Rust replacement** for the `pve` bash tool —
every `pve` command is reimplemented over the existing SSH transport, with **no
shelling out to `./pve`**. Once at parity, `pve` is retired. This resolves the
former open product question; the roadmap below is the parity checklist.

### `pve` command surface to reach parity with

The sibling tool is one ~38 KB bash script (`~/Code/proxmox-vm-helper/pve`) with
these subcommands — each must land natively in `mox`:

**Full parity reached** — every `pve` subcommand now has a native mox
implementation; `pve` can be retired:

| `pve` command | Purpose | mox status |
|---|---|---|
| `list` | list VMs | ✅ `mox list` + TUI |
| `status VM` | cores/mem/disk/state | ✅ detail overlay (Enter) |
| `ip VM` | LAN + Tailscale IP via guest agent | ✅ detail overlay (Enter) |
| `start` / `stop` / `destroy` | lifecycle | ✅ `s`/`x`/`d` + confirm modal |
| `create NAME [opts]` | clone→configure→boot→wait-for-IP | ✅ create wizard (`c`) |
| `init` | config wizard → writes config | ✅ `mox init` |
| `setup-access` | gen SSH key, install pubkey, `~/.ssh/config` block | ✅ `mox setup-access` |
| `doctor` | check connectivity, storage/bridge, `virt-customize` | ✅ `mox doctor` |
| `template` | download cloud image, `virt-customize`, make template | ✅ `mox template` |

Every command is native. What remains is roadmap **#7 polish** (see below) —
nothing blocks retiring `pve`.

`create` options to preserve: `--id --cores --memory --disk --ip DHCP|CIDR
--gateway --nameserver --user --ssh-key --full --no-start --tailscale
--no-tailscale --no-wait --tags`. Port faithfully from the bash implementation —
read the corresponding function in `pve` before writing each one.

## Roadmap (in order)

1. ~~**Standalone config (`init`).**~~ ✅ **done.** Discovery order is now
   `$PVE_CONFIG_FILE` → nearest `.env` up from cwd → `~/.config/mox/config` →
   `~/.config/pve/.env`. `mox init` (`src/init.rs`) is an interactive wizard:
   prompts for connection/subnet/storage/bridge/tailscale (prefilled from any
   existing config, detects storages/bridges when access works), backs up any
   existing file, and writes `~/.config/mox/config`. Config parsing was
   refactored into `Config::parse_env` over `Config::defaults()`, with
   `Config::to_env_string` as the serializer (round-trip unit-tested). INTERACTIVE
   + writes to `$HOME`, so not run live from here; pure parts are unit-tested.
2. ~~**Detail pane (`status` + `ip`)**~~ — ✅ **done.** `Enter` opens an overlay
   with cores/mem/disk/uptime + LAN/Tailscale IPs via the guest agent. Possible
   follow-ups: cloud-init / tailscale health lines (`vm_tailscale_error` in the
   bash tool diagnoses why Tailscale didn't come up — a `qm guest exec … tailscale
   status --json` call worth porting later).
3. ~~**Actions (`start`/`stop`/`destroy`)**~~ — ✅ **done.** `s`/`x`/`d` +
   confirmation modal, `qm` over SSH, live status reflect via `Req::Action` /
   `Update::ActionDone`. Established the mutating-command pattern (confirm modal +
   fetcher-thread dispatch) that create/template reuse.
4. **Create (`create` + `template`)** — the big one; decomposed:
   - **4a — node introspection (read-only).** ✅ **done.** `Node::next_id`
     (`pvesh get /cluster/nextid`), `storages` (`pvesm status`), `bridges`
     (`ip -brief link show type bridge`). Feeds the create form's dropdowns +
     validation. Verified live: next_id 101, storages local/local-lvm/
     synology-iso, bridge vmbr0.
   - **4b — create (clone + configure + start).** ✅ **done.** The bash `create`
     REMOTE script is embedded **verbatim** at `src/create.sh` (via
     `include_str!`) and run through `Node::run_script` — the script is piped to
     `bash -s -- <17 positional args>` over SSH (mirrors `remote_bash`), so the
     VM name/tags can't shell-inject. `CreateSpec` + `create_args` assemble the
     args; key material is base64'd by a tiny no-dep encoder (RFC-4648 tested).
     A TUI wizard (`c` in the list) collects the fields and shows
     editing → submitting → outcome. MUTATING — verified end-to-end WITHOUT
     mutation by running `create.sh` with a bogus template id (tripped the first
     validation guard, exit 1). The actual clone→start was deliberately NOT
     fired at the real node.
   - **4c — tailscale vendor-data + wait-for-IP + ssh-config.** ✅ **done.** The
     tailscale cloud-init vendor snippet + `--auth-key=file:` are inside the
     embedded `create.sh`; `Node::wait_for_ips` polls `guest_ips` after a started
     create to fill the outcome's LAN/Tailscale IPs; and `src/sshconfig.rs`
     writes an idempotent `~/.ssh/config` `Host` block (own `# mox-managed`
     marker so it never disturbs pve's `# pve-managed` blocks) for a started
     Tailscale VM, so the outcome screen can offer `ssh <name>`. Ported from
     `add_ssh_config_vm` / `remove_ssh_config_vm`; unit-tested against a temp
     file (never the real `~/.ssh/config`).
   - **4d — template build.** ✅ **done.** `mox template` CLI verb runs the
     bash tool's template script embedded verbatim at `src/template.sh`
     (download cloud image → `virt-customize` bakes qemu-guest-agent + tailscale
     + zeroes machine-id → `qm create`/importdisk/convert to template).
     Long-running, so it uses `Node::run_script_streamed` (inherits stdout so
     progress streams live). Idempotent: exits early if the template exists.
     Config gained the template inputs (name/image_url/storage/cpu_type/
     packages); `template_package_list` computes floor + extras (unit-tested).
     MUTATING + multi-GB download — NOT run live (also blocked by the auto-mode
     classifier); verified the idempotency guard's precondition read-only
     (`qm config 9000` → `template: 1`), so a live run would early-exit safely.
   - Interface still open: CLI `mox create NAME [flags]` (mirrors pve flags) vs a
     TUI wizard form. Streaming progress can poll a Proxmox task UPID.
5. ~~**Host bootstrap (`setup-access`)**~~ — ✅ **done.** `mox setup-access` CLI
   verb (`src/setup.rs`): generates `~/.ssh/proxmox/id_ed25519` via `ssh-keygen`
   if missing, installs the pubkey with `ssh-copy-id` (inherited stdio → the
   one-time password prompt works), wires the idempotent `# mox-managed subnet`
   block in `~/.ssh/config` (`sshconfig::ensure_subnet_block`, needs
   `PVE_VM_SUBNET`), then verifies via `node_info`. INTERACTIVE + mutates local
   `~/.ssh` and the node's `authorized_keys`, so NOT live-tested from here — the
   user runs it. Pure parts (subnet→`Host` glob, idempotency) are unit-tested.
6. ~~**Preflight (`doctor`)**~~ — ✅ **done.** `mox doctor` (read-only CLI verb):
   prints the connection summary from config, then the node's hostname, Proxmox
   version, `virt-customize` presence, storages, and bridges via `Node::node_info`
   + `storages`/`bridges` (retired their `#[allow(dead_code)]`). Verified live
   against the real node. Only `next_id` remains parked (for the form ID hint).
7. **Polish & packaging** — in progress:
   - ✅ `--json` on `list` and `doctor` (`Vm`/`NodeInfo` derive `Serialize`;
     doctor emits a `{connection, node}` object). Verified live.
   - ✅ create-form **next-id hint** — `Req::NextId`/`Update::NextId` resolve
     `pvesh /cluster/nextid` async; the blank VM ID field shows
     "(next available: N)". Retired the last `#[allow(dead_code)]`.
   - ✅ lunar touch — `☾ mox` in the TUI header.
   - ✅ shell completions — `mox completions <bash|zsh|fish|elvish|powershell>`
     prints a completion script (via `clap_complete`, +1 dep, builds on the
     existing clap so no transitive bloat). E.g. `mox completions zsh >
     ~/.zfunc/_mox`.
   - ✅ crates.io packaging prep — `Cargo.toml` has `rust-version` (1.74),
     `readme`, `keywords`, `categories`. `cargo package` builds a valid crate
     (verified; `src/*.sh` are bundled for `include_str!`). Release binary is
     stripped + LTO'd. REMAINING before `cargo publish`: set `repository` in
     `Cargo.toml` to your git URL (left as a TODO — not fabricated), and a
     crates.io account/token. A `brew` formula/tap is still open (out-of-repo).

## Working in this repo

- Rust was installed via `rustup`. In a fresh non-login shell you may need
  `source "$HOME/.cargo/env"` before `cargo`.
- Build: `cargo build`. Run: `./target/debug/mox [list]`.
- **Verify TUI changes headlessly:** `cargo test -- --nocapture` renders the
  dashboard via `TestBackend` and prints the buffer. A full-screen TUI can't be
  driven from a non-interactive shell, so use this instead of trying to run it.
- Live check against the real node:
  `PVE_CONFIG_FILE=~/Code/proxmox-vm-helper/.env ./target/debug/mox list`.
- Keep `main.rs`'s no-subcommand path launching the TUI; add new verbs as `clap`
  subcommands.
