#!/bin/zsh
# Builds "Auroval File Spike.app" (ad-hoc signed) into build/.
set -euo pipefail
cd "${0:A:h}"
app="build/Auroval File Spike.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp Info.plist "$app/Contents/"
swiftc -O -o "$app/Contents/MacOS/spike" spike.swift
codesign --force -s - "$app"
echo "built $app"
