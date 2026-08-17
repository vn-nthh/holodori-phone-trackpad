#!/usr/bin/env bash
# Launches the Doritrack controller in keyboard mode (the only mode
# available on Linux; see README.txt).
set -euo pipefail
cd "$(dirname "$0")"
exec ./HolodoriUsbController
