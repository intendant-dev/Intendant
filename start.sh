#!/bin/bash
# Automatisches Update und Start-Skript für Intendant

# Sicherstellen, dass wir im richtigen Verzeichnis sind
cd "$(dirname "$0")" || exit

echo "--- Pulling latest changes ---"
git pull

echo "--- Building release ---"
cargo build --release

if [ $? -eq 0 ]; then
    echo "--- Starting Intendant ---"
    ./target/release/intendant "$@"
else
    echo "Error: Build failed."
    exit 1
fi
