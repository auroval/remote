#!/usr/bin/env bash
# Prepares a GitHub Ubuntu runner for packaging/flatpak/build.sh.
set -euo pipefail
sudo apt-get update -qq
sudo apt-get install -y -qq flatpak flatpak-builder
# Ubuntu's AppArmor blocks the unprivileged user namespaces bubblewrap needs.
sudo sysctl -qw kernel.apparmor_restrict_unprivileged_userns=0
