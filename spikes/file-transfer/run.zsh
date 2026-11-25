#!/bin/zsh
# Runs one spike command in the app (so macOS attributes permissions to the
# app, not the terminal) and prints its log. Usage: ./run.zsh a <file> | b | d | e <file>
set -euo pipefail
cd "${0:A:h}"
: > /tmp/auroval-file-spike.log
open -W -n "build/Auroval File Spike.app" --args "$@"
cat /tmp/auroval-file-spike.log
