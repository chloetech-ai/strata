#!/usr/bin/env bash
# Starts CI's runners on a droplet made from the ci-runner-base snapshot
# (provision.sh). The controller (.github/ci-runner/ci_runner.py) creates
# the droplet with the volume `ci-runner-cache` attached and user data that
# runs:  boot.sh <registration token> <label> <runners>
#
# The volume keeps the runners' work folders, the cargo registry and the
# build folders (CARGO_TARGET_DIR, outside the checkout) from one droplet to
# the next: the droplet itself is destroyed when CI has been idle a while,
# so a powered-off droplet never bills.
set -euo pipefail
exec > >(tee -a /var/log/ci-runner-boot.log) 2>&1
# A droplet whose runners cannot start is of no use: power it off, which the
# controller and the reaper see (status "off") and act on at once, rather
# than leave it billing until it times out.
trap 'echo "boot.sh failed at line $LINENO"; poweroff' ERR
token="$1" label="$2" count="$3"
repo_url=https://github.com/chloetech-ai/chloedb

# Jobs must not read the user data (the registration token) back from the
# metadata service.
iptables -I OUTPUT -d 169.254.169.254 -m owner --uid-owner runner -j REJECT

dev=/dev/disk/by-id/scsi-0DO_Volume_ci-runner-cache
for i in $(seq 1 60); do [ -e "$dev" ] && break; sleep 1; done
blkid "$dev" >/dev/null 2>&1 || mkfs.ext4 -q "$dev"
mkdir -p /mnt/ci
mount -o discard,defaults,noatime "$dev" /mnt/ci
install -d -o runner -g runner /mnt/ci/cargo/registry /mnt/ci/cargo/git /mnt/ci/sccache
for d in registry git; do
  rm -rf "/home/runner/.cargo/$d"
  ln -s "/mnt/ci/cargo/$d" "/home/runner/.cargo/$d"
done
# A volume that fills up fails builds: drop the oldest build folders first.
use=$(df --output=pcent /mnt/ci | tail -1 | tr -dc 0-9)
if [ "$use" -gt 80 ]; then
  find /mnt/ci -maxdepth 3 -path '*/target/*' -type d -printf '%T@ %p\n' | sort -n | head -n 4 | cut -d' ' -f2- | xargs -r rm -rf
fi

install -d /opt/runners
for i in $(seq 1 "$count"); do
  dir=/opt/runners/$i
  rm -rf "$dir"
  cp -a /opt/actions-runner "$dir"
  install -d -o runner -g runner "/mnt/ci/work-$i"
  # Each runner its own rustup and cargo homes, copied from the image's:
  # two jobs starting at once on a fresh droplet ran rustup over the same
  # ~/.cargo/bin, and one died of "Text file busy" on cargo while the other
  # could not spawn rustc (11-14). The registry stays shared on the volume;
  # cargo locks it.
  rh=/opt/rust/$i
  rm -rf "$rh"
  install -d "$rh/cargo"
  cp -a /home/runner/.rustup "$rh/rustup"
  cp -a /home/runner/.cargo/bin "$rh/cargo/bin"
  for d in registry git; do ln -s "/mnt/ci/cargo/$d" "$rh/cargo/$d"; done
  chown -R runner:runner "$rh" "$dir"
  cd "$dir"
  sudo -u runner ./config.sh --unattended --replace --url "$repo_url" --token "$token" \
    --name "do-$(hostname)-$i" --labels "$label" --work "/mnt/ci/work-$i" --no-default-labels
  cat >> "$dir/.env" <<ENV
ACTIONS_RUNNER_HOOK_JOB_STARTED=/opt/ci-runner/job-started.sh
CI_RUNNER_CACHE=/mnt/ci/work-$i
RUSTUP_HOME=$rh/rustup
CARGO_HOME=$rh/cargo
ENV
  # The runner's PATH (written by config.sh) finds this runner's cargo first.
  echo "$rh/cargo/bin:$(cat "$dir/.path" 2>/dev/null || echo /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin)" > "$dir/.path"
  chown runner:runner "$dir/.path"
  cat > "/etc/systemd/system/ci-runner-$i.service" <<UNIT
[Unit]
Description=GitHub Actions runner $i
After=network-online.target
[Service]
User=runner
WorkingDirectory=$dir
ExecStart=$dir/run.sh
Restart=always
KillSignal=SIGINT
[Install]
WantedBy=multi-user.target
UNIT
done
systemctl daemon-reload
for i in $(seq 1 "$count"); do systemctl start "ci-runner-$i"; done
# The token is spent; it expires within the hour anyway.
rm -f /var/lib/cloud/instance/user-data.txt /var/lib/cloud/instance/user-data.txt.i /var/lib/cloud/instance/scripts/runcmd 2>/dev/null || true
