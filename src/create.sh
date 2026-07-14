set -euo pipefail

template_id="$1"
vmid="$2"
name="$3"
cores="$4"
memory="$5"
disk="$6"
ip="$7"
gateway="$8"
nameserver="$9"
ci_user="${10}"
full="${11}"
start="${12}"
tags="${13}"
key_base64="${14}"
tailscale="${15}"
authkey_base64="${16}"
tailnet_domain="${17}"

qm status "$template_id" >/dev/null 2>&1 || {
  printf 'Template %s does not exist. Run ./pve template.\n' "$template_id" >&2
  exit 1
}
[[ "$(qm config "$template_id" | awk '/^template:/ {print $2}')" == 1 ]] || {
  printf 'VM %s is not a template.\n' "$template_id" >&2
  exit 1
}

if [[ -z "$vmid" ]]; then
  vmid="$(pvesh get /cluster/nextid)"
fi
if qm status "$vmid" >/dev/null 2>&1; then
  printf 'VM ID %s already exists.\n' "$vmid" >&2
  exit 1
fi
if qm list | awk -v wanted="$name" 'NR > 1 && $2 == wanted {found=1} END {exit !found}'; then
  printf 'A VM named %s already exists.\n' "$name" >&2
  exit 1
fi

# Ensure snippet storage exists for vendor-data (Tailscale join).
if ! pvesm status --content snippets 2>/dev/null | grep -q '^local'; then
  pvesm set local --content snippets,iso,vztmpl,backup,import
fi
snippet_dir="$(pvesm path local 2>/dev/null || echo '')"
[[ -z "$snippet_dir" ]] && snippet_dir=/var/lib/vz
snippet_dir="$snippet_dir/snippets"
mkdir -p "$snippet_dir"

key_file="$(mktemp)"
printf '%s' "$key_base64" | base64 --decode > "$key_file"
chmod 600 "$key_file"

vendor_snippet=""
authkey_file=""
cleanup() {
  status=$?
  rm -f "$key_file" "$authkey_file"
  if [[ "$status" != 0 && "${created:-0}" == 1 ]]; then
    printf 'VM creation failed; removing partial VM %s.\n' "$vmid" >&2
    qm stop "$vmid" >/dev/null 2>&1 || true
    qm destroy "$vmid" --purge 1 >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT

created=0

printf 'Cloning template %s to VM %s (%s)...\n' "$template_id" "$vmid" "$name"
qm clone "$template_id" "$vmid" --name "$name" --full "$full"
created=1

qm set "$vmid" --cores "$cores" --memory "$memory"
qm set "$vmid" --ciuser "$ci_user" --sshkeys "$key_file"

ipconfig="ip=$ip"
if [[ -n "$gateway" ]]; then
  ipconfig="$ipconfig,gw=$gateway"
fi
qm set "$vmid" --ipconfig0 "$ipconfig"
if [[ -n "$nameserver" ]]; then
  qm set "$vmid" --nameserver "$nameserver"
fi
if [[ -n "$tags" ]]; then
  qm set "$vmid" --tags "$tags"
fi

# Tailscale join via cloud-init vendor-data (runs alongside generated
# user-data, so --ciuser/--sshkeys/--ipconfig0 are preserved). The auth key is
# written to a root-only file and consumed via --auth-key=file: so it never
# appears in process lists or shell history.
if [[ "$tailscale" == 1 ]]; then
  authkey_file="$(mktemp)"
  printf '%s' "$authkey_base64" | base64 --decode > "$authkey_file"
  chmod 600 "$authkey_file"
  authkey_inject="/root/.ts-authkey"
  vendor_snippet="$snippet_dir/vendor-$vmid.yml"
  cat >"$vendor_snippet" <<YAML
#cloud-config
# pve-managed vendor-data for VM $vmid ($name). Removed by ./pve destroy.
write_files:
  - path: $authkey_inject
    permissions: '0600'
    owner: root:root
    content: |
$(sed -e 's/^/      /' "$authkey_file")
runcmd:
  - [ sh, -c, "until command -v tailscale >/dev/null 2>&1; do sleep 2; done" ]
  - [ tailscale, up, --auth-key=file:$authkey_inject, --hostname=$name, --accept-routes ]
  - [ sh, -c, "rm -f $authkey_inject" ]
YAML
  rm -f "$authkey_file"
  qm set "$vmid" --cicustom "vendor=local:snippets/vendor-$vmid.yml"
  printf 'Configured Tailscale join (vendor-data) for %s.\n' "$name"
fi

qm resize "$vmid" scsi0 "${disk}G"

if [[ "$start" == 1 ]]; then
  qm start "$vmid"
fi

created=0
# Machine-parseable summary. IP/Tailscale fields are filled in by the caller
# after waiting for the guest agent.
printf 'VM_ID=%s\nVM_NAME=%s\nVM_USER=%s\nSTARTED=%s\nTAILSCALE=%s\nTAILNET_DOMAIN=%s\n' \
  "$vmid" "$name" "$ci_user" "$start" "$tailscale" "$tailnet_domain"
