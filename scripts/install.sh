#!/bin/sh
set -eu
lucy_project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
lucy_install_dir="${HOME:?}/.local/bin"
cargo build --manifest-path "$lucy_project_dir/Cargo.toml" \
    --target-dir "$lucy_project_dir/target" --bin lucy-shell --release --locked
install -Dm755 "$lucy_project_dir/target/release/lucy-shell" "$lucy_install_dir/lucy-shell.new"
mv "$lucy_install_dir/lucy-shell.new" "$lucy_install_dir/lucy-shell"
printf '%s\n' "Lucy installed: $lucy_install_dir/lucy-shell"
