#!/usr/bin/env bash
# Installs what CI's self-hosted runner needs on a fresh Ubuntu 24.04
# droplet: the build tools, Rust (the RUST_TOOLCHAIN of ci.yml), Python,
# the GitHub Actions runner, its job-started hook and the boot script.
# The droplet is then powered off and snapshotted as `ci-runner-base-<date>`
# (.github/workflows/ci-runner-image.yml); every CI droplet starts from the
# newest such snapshot. Nothing here is secret, and nothing secret is left
# on the image: the registration token comes with each droplet's user data.
set -euxo pipefail
TOOLCHAIN="${1:-1.100.0}"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends build-essential pkg-config git curl ca-certificates jq unzip \
  python3 python3-venv python3-pip libssl-dev clang lld
# Postgres's programs, for chloedb-server's tests/postgres.rs, which starts
# its own from initdb and, in CI, fails when there is none (audit 4,
# X-I3). Installed, the package's scripts start its service, which hung
# this image's build until its deadline (#470): its libraries are
# installed, and the package itself only unpacked, so no script runs.
apt-get install -y --no-install-recommends tzdata libgssapi-krb5-2 libicu74 libldap2 liblz4-1 libpam0g libpq5 \
  libselinux1 libssl3t64 libsystemd0 libuuid1 libxml2 libxslt1.1 libzstd1 zlib1g
(cd /tmp && apt-get download postgresql-16 && dpkg -x postgresql-16_*.deb / && rm -f postgresql-16_*.deb)
/usr/lib/postgresql/16/bin/initdb --version
/usr/lib/postgresql/16/bin/postgres --version
id runner >/dev/null 2>&1 || useradd -m -s /bin/bash runner
# Rust for the runner user; dtolnay/rust-toolchain in each job installs the
# toolchain the job names, which is this one, so it is already there.
sudo -u runner bash -c "curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain $TOOLCHAIN -c clippy"
# sccache, a compile cache shared by both runners on the volume
# (target-dir.sh turns it on): a crate compiled once with the same inputs
# is not compiled again, in any job or build folder.
curl -sSfL https://github.com/mozilla/sccache/releases/download/v0.10.0/sccache-v0.10.0-x86_64-unknown-linux-musl.tar.gz \
  | tar -xz -C /usr/local/bin --strip-components=1 sccache-v0.10.0-x86_64-unknown-linux-musl/sccache
chmod 0755 /usr/local/bin/sccache
# The Actions runner, unconfigured: boot.sh registers it.
ver=$(curl -sSf https://api.github.com/repos/actions/runner/releases/latest | jq -r .tag_name | sed 's/^v//')
mkdir -p /opt/actions-runner
curl -sSfL "https://github.com/actions/runner/releases/download/v${ver}/actions-runner-linux-x64-${ver}.tar.gz" | tar -xz -C /opt/actions-runner
/opt/actions-runner/bin/installdependencies.sh
chown -R runner:runner /opt/actions-runner
install -d /opt/ci-runner
install -m 0755 "$(dirname "$0")/boot.sh" /opt/ci-runner/boot.sh
install -m 0755 "$(dirname "$0")/job-started.sh" /opt/ci-runner/job-started.sh
# No sshd: nothing logs in to a CI droplet. Its console is DigitalOcean's.
systemctl disable --now ssh.socket ssh.service 2>/dev/null || true
cloud-init clean --logs
