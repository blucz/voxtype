#!/usr/bin/env bash
# Build and atomically install the local fork under the user's account.
# Package-owned executables remain available for rollback; no sudo needed.
set -euo pipefail
SRC="${VOXTYPE_SRC:-$HOME/omarchy/voxtype}"
ROOT="$HOME/.local/libexec/voxtype"
FEATURES=parakeet,moonshine,sensevoice,paraformer,dolphin,omnilingual,cohere,ml-diarization,osd-gtk4
cd "$SRC"
if [[ "${1:-}" != --install-only ]]; then
  env -u ARGV0 CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}" cargo build --locked --release --features "$FEATURES" --bin voxtype --bin voxtype-osd --bin voxtype-osd-gtk4
fi
runtime="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/voxtype/state"
if [[ -f "$runtime" && "$(cat "$runtime")" != idle ]]; then
  echo 'A dictation is active; finish it before installing. Nothing has been restarted.' >&2
  exit 75
fi
release="$ROOT/releases/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$release" "$HOME/.local/bin" "$HOME/.config/systemd/user/voxtype.service.d"
for binary in voxtype voxtype-osd voxtype-osd-gtk4; do
  install -m755 "target/release/$binary" "$release/$binary"
done
sha256sum "$release"/* > "$release/SHA256SUMS"
# The previous release remains on disk. Switching current is one rename.
systemctl --user stop voxtype
ln -s "$release" "$ROOT/.current-$$"
mv -Tf "$ROOT/.current-$$" "$ROOT/current"
for binary in voxtype voxtype-osd voxtype-osd-gtk4; do
  ln -sfn "$ROOT/current/$binary" "$HOME/.local/bin/$binary"
done
cat > "$HOME/.config/systemd/user/voxtype.service.d/local-build.conf" <<'UNIT'
[Service]
ExecStart=
ExecStart=%h/.local/libexec/voxtype/current/voxtype daemon
UNIT
systemctl --user daemon-reload
systemctl --user start voxtype
"$ROOT/current/voxtype" --version
printf 'Installed %s\n' "$release"
