#!/usr/bin/env bash
# Run from an extracted release bundle. Existing configuration is preserved.
set -euo pipefail
bundle=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if [[ $(id -u) -ne 0 ]]; then
    echo 'Run this installer as root on the target host.' >&2
    exit 1
fi
cd "$bundle"
sha256sum --check SHA256SUMS
if ! getent group logshipper > /dev/null; then
    groupadd --system logshipper
fi
if ! getent passwd logshipper > /dev/null; then
    useradd --system --gid logshipper --home-dir /var/lib/logshipper --shell /usr/sbin/nologin logshipper
fi
install -d -m 0755 /usr/local/bin /usr/local/share/doc/logshipper
install -d -m 0750 -o root -g logshipper /etc/logshipper
install -d -m 0700 -o logshipper -g logshipper /var/lib/logshipper
# Atomic binary replacement, retaining the previous executable for controlled rollback.
if [[ -f /usr/local/bin/logshipper ]]; then
    install -m 0755 /usr/local/bin/logshipper /usr/local/bin/logshipper.previous
fi
install -m 0755 logshipper /usr/local/bin/logshipper.new
mv -f /usr/local/bin/logshipper.new /usr/local/bin/logshipper
install -m 0644 packaging/logshipper.service /etc/systemd/system/logshipper.service
install -m 0644 README.md DEPLOYMENT.md build-info.json /usr/local/share/doc/logshipper/
install -m 0644 packaging/alerts.yml /usr/local/share/doc/logshipper/alerts.yml
install -m 0644 restore-stream.py /usr/local/share/doc/logshipper/restore-stream.py
if [[ ! -e /etc/logshipper/config.toml ]]; then
    install -m 0640 -o root -g logshipper config.example.toml /etc/logshipper/config.toml
fi
systemctl daemon-reload
echo 'Installed. Configure /etc/logshipper/config.toml and source/export permissions before enabling the service.'
echo 'Existing services are not restarted automatically. Follow DEPLOYMENT.md for upgrade and rollback.'
