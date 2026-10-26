#!/usr/bin/env bash
set -euo pipefail
DEST="${1:-$HOME/cc/ledger}"
rm -rf "$DEST"
git clone -q "$HOME/cc/ledger-pristine" "$DEST"
echo "fresh practice repo at $DEST"
