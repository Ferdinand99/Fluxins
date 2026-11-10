#!/usr/bin/env bash
# Writes THIRD-PARTY-NOTICES at the repository root with cargo-about
# (https://github.com/EmbarkStudios/cargo-about), from about.toml and about.hbs.
# The file is committed (the app embeds it in its About view) and shipped next to
# every binary. Run it again after any dependency change.
#
#   cargo install cargo-about --locked --version 0.9.3 --features cli   # once
#   scripts/third-party-notices.sh
set -euo pipefail
cd "$(dirname "$0")/.."
cargo about generate --locked --manifest-path crates/inlet-app/Cargo.toml --features demo about.hbs -o THIRD-PARTY-NOTICES
echo "wrote THIRD-PARTY-NOTICES ($(wc -l < THIRD-PARTY-NOTICES) lines)"
