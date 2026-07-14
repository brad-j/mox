set -euo pipefail

template_id="$1"
template_name="$2"
image_url="$3"
storage="$4"
bridge="$5"
cpu_type="$6"
ci_user="$7"
packages="$8"
ts_install_url="$9"

if qm status "$template_id" >/dev/null 2>&1; then
  if qm config "$template_id" | grep -q '^template: 1$'; then
    printf 'Template %s already exists; nothing to do.\n' "$template_id"
    exit 0
  fi
  printf 'VM ID %s already exists but is not a template.\n' "$template_id" >&2
  exit 1
fi

pvesm status --storage "$storage" >/dev/null 2>&1 || {
  printf 'Storage %s does not exist. Run ./pve doctor and update .env.\n' "$storage" >&2
  exit 1
}

# Ensure 'local' storage can hold cloud-init snippets (vendor-data for the
# Tailscale join). Idempotent.
if ! pvesm status --content snippets 2>/dev/null | grep -q '^local'; then
  printf 'Enabling snippets content type on local storage...\n'
  pvesm set local --content snippets,iso,vztmpl,backup,import
fi

image_dir=/var/lib/vz/template/iso
image_file="$image_dir/$(basename "$image_url")"
mkdir -p "$image_dir"

if [[ ! -s "$image_file" ]]; then
  printf 'Downloading %s...\n' "$image_url"
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --output "$image_file.part" "$image_url"
  else
    wget --output-document="$image_file.part" "$image_url"
  fi
  mv "$image_file.part" "$image_file"
else
  printf 'Using cached image %s\n' "$image_file"
fi

# Ensure virt-customize is available (libguestfs-tools). Auto-install once.
if ! command -v virt-customize >/dev/null 2>&1; then
  printf 'Installing libguestfs-tools (provides virt-customize)...\n'
  apt-get update -qq
  apt-get install -y -qq libguestfs-tools
fi

printf 'Installing base packages into image: %s + tailscale\n' "$packages"
# virt-customize --install runs apt-get update internally. Work on a copy so
# the cached pristine image is preserved for re-runs.
cp "$image_file" "$image_file.customized"
# Install the apt packages (qemu-guest-agent + user packages).
virt-customize -a "$image_file.customized" --install "$packages"
# Install tailscale via its official install.sh, which adds the Tailscale
# apt repo + key (tailscale is not in Ubuntu's default repos).
virt-customize -a "$image_file.customized" \
  --run-command "curl -fsSL $ts_install_url | sh"
# Zero the image's machine-id so every clone regenerates a unique one on first
# boot. Otherwise all clones present the same systemd-derived DHCP client-id and
# collide on a single DHCP lease (identical IP despite differing MACs).
virt-customize -a "$image_file.customized" \
  --run-command "truncate -s 0 /etc/machine-id; rm -f /var/lib/dbus/machine-id; ln -sf /etc/machine-id /var/lib/dbus/machine-id"
mv "$image_file.customized" "$image_file"

created=0
cleanup_on_error() {
  status=$?
  if [[ "$created" == 1 ]]; then
    printf 'Template creation failed; removing partial VM %s.\n' "$template_id" >&2
    qm destroy "$template_id" --purge 1 >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup_on_error ERR

qm create "$template_id" \
  --name "$template_name" \
  --ostype l26 \
  --cpu "$cpu_type" \
  --cores 2 \
  --memory 2048 \
  --net0 "virtio,bridge=$bridge" \
  --scsihw virtio-scsi-pci
created=1

qm importdisk "$template_id" "$image_file" "$storage"
imported_disk="$(qm config "$template_id" | awk -F': ' '/^unused[0-9]+:/ {print $2; exit}')"
[[ -n "$imported_disk" ]] || {
  printf 'Could not identify the imported cloud-image disk.\n' >&2
  exit 1
}

qm set "$template_id" --scsi0 "$imported_disk,discard=on"
qm set "$template_id" --ide2 "$storage:cloudinit"
qm set "$template_id" --boot order=scsi0
qm set "$template_id" --serial0 socket --vga serial0
qm set "$template_id" --agent enabled=1,fstrim_cloned_disks=1
qm set "$template_id" --ciuser "$ci_user" --ipconfig0 ip=dhcp
qm resize "$template_id" scsi0 8G
qm template "$template_id"

created=0
trap - ERR
printf 'Created template %s (%s) with base packages: %s,tailscale\n' "$template_name" "$template_id" "$packages"
