# Local Omarchy deployment

This directory records the user-owned installation used with this fork. It
requires the normal Voxtype build dependencies, GTK4/layer-shell, the existing
Omarchy environment, and the locally quantized Parakeet model. It does not
install system packages or download models.

From the checkout, run:

```sh
bash local/omarchy/install-local.sh
```

`VOXTYPE_SRC` can override the default checkout (`~/omarchy/voxtype`). The
installer builds all three binaries together, retains versioned releases under
`~/.local/libexec/voxtype/releases/`, atomically changes `current`, updates
`~/.local/bin/voxtype*`, and installs the user-service drop-in shown in
`local-build.conf`. It refuses to restart during an active dictation. To keep
the convenient rebuild command, copy `install-local.sh` to
`~/.local/bin/voxtype-install-local` and make it executable.

Merge the Voxtype settings from `config.toml` into the existing user config,
and the bindings from `bindings.lua` into `~/.config/hypr/bindings.lua`, replacing
the old recording submap and its bindings. Remove its pre/post hook commands
from the Voxtype config. Validate Hyprland with `hyprctl reload` followed by
`hyprctl configerrors`. These files contain only dictation settings.

Ctrl+Esc twice within 1.5 seconds cancels with transcript recovery. Plain
Escape and normal desktop shortcuts stay available. F9 and Super+Ctrl+X retain
their recording functions. The 480x82 preview shows two lines; its left grip
moves it and the right minus/plus collapses/expands it. Layout is stored in
`~/.local/state/voxtype/osd-layout.json`. Dragging uses absolute Hyprland cursor
coordinates so moving the panel does not feed back into the drag offset.

The corral is a separate local application. Its existing Voxtype journal
importer adds completed and cancelled transcripts for 30 minutes; that
application and the microphone/model data are not bundled here.

## Validation

The change passed 1,062 library tests (two ignored), seven headless overlay
tests, a GTK drag/collapse handler test, an actual-model whitespace-boundary
check, and isolated focus/cancellation checks. The model check is reproducible:

```sh
cargo run --release --features parakeet --example stream-boundary-check -- \
  ~/.config/voxtype/config.toml \
  ~/.local/share/voxtype/models/parakeet-unified-en-0.6b-int8
```

Full-feature Clippy has pre-existing warnings in the fork. The changed code was
checked with those existing lint categories allowed; no unrelated lint cleanup
is included here.

## Rollback

While idle, repoint `current` to a previous release and restart the user service.
The prior package-path binaries are retained. To return to those, restore the
previous config/bindings, remove the three `~/.local/bin/voxtype*` symlinks and
`voxtype.service.d/local-build.conf`, reload systemd, and restart while idle.
On the original installation, the backup location is recorded in
`~/.local/state/voxtype-update/backup-path`.
