#!/usr/bin/env bash
# Build both platforms and publish the version from Cargo.toml as a GitHub release.
# Running apps offer the update on their next start (Edit Mode toolbar).
set -euo pipefail
cd "$(dirname "$0")"
v=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
git diff --quiet && git diff --cached --quiet || { echo "Commit your changes first."; exit 1; }
git rev-parse "v$v" >/dev/null 2>&1 && { echo "v$v already exists: bump the version in Cargo.toml."; exit 1; }
cargo test --release
cargo about generate about.hbs -o THIRD_PARTY_LICENSES.txt
git diff --quiet THIRD_PARTY_LICENSES.txt || { echo "Dependency licenses changed: commit THIRD_PARTY_LICENSES.txt first."; exit 1; }
cargo build --release
cargo zigbuild --release --target x86_64-pc-windows-gnu
mkdir -p dist
cp target/x86_64-pc-windows-gnu/release/cuespark.exe dist/CueSpark.exe
cp target/release/cuespark dist/cuespark-linux-x86_64
git tag "v$v"
git push origin HEAD "v$v"
gh release create "v$v" dist/CueSpark.exe dist/cuespark-linux-x86_64 --title "Cue Spark $v" --generate-notes
