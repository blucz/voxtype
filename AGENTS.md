# Agent notes for this fork

This checkout is **blucz/voxtype**, a fork of upstream voxtype maintained for
one machine (an Omarchy / Hyprland laptop). `CLAUDE.md` in this directory is
upstream's and describes the project itself; this file describes only how this
fork differs and how it is deployed. Keep them separate — editing upstream's
`CLAUDE.md` creates rebase conflicts for no benefit.

## The deployment is unusual

Binaries built here are installed *over* two files owned by the Arch package
`voxtype-bin`, rather than replacing the package:

- `target/release/voxtype` → `/usr/lib/voxtype/voxtype-onnx-avx2`
- `target/release/voxtype-osd-gtk4` → `/usr/lib/voxtype/voxtype-osd-gtk4`

So `voxtype --version` reports this fork's version while `pacman -Qo` still
credits `voxtype-bin`. That is expected, not corruption. `/etc/pacman.conf`
has `NoUpgrade` entries for both paths so upgrades leave them alone and drop a
`.pacnew` beside them instead.

Rebuild and reinstall with `~/.local/bin/voxtype-install-local`. Building
requires `cmake`, because `whisper-rs` is a hard dependency and compiles even
when only the OSD binary is wanted.

Full details, including why a `replaces=voxtype-bin` package was rejected:
**`~/.config/omarchy/LOCAL-CUSTOMIZATIONS.md`** on that machine.

## What diverges from upstream

Upstream bugs, worth submitting as PRs:

- Parakeet streaming windows must be multiples of 0.08s (mel-frame count
  divisible by 8 at a 10ms hop). Upstream defaults 0.5/1.5/0.5 are invalid, so
  enabling streaming crash-loops the daemon at startup. Now 0.48/1.6/0.48.
- `position = "top-center"` was routed through the fractional placement path
  used by `bottom-center` and rendered at the bottom of the screen.

Behaviour changes worth submitting upstream:

- `audio.max_duration_secs = 0` now means "no limit". A cap that stops a
  recording *and delivers the text* is destructive by design: it ends
  mid-sentence, pastes into whatever window has focus, and silently drops
  everything said afterwards. The cap still works at any non-zero value.
- `voxtype record cancel --double-tap`: the first press only arms (and shows
  a prompt in the OSD), the second within the window discards. Written for
  Escape, which is far too easy to hit by accident to be a one-press
  destroy. Arm state lives in a runtime file, since the caller is a
  short-lived compositor keybind.
- A transcript journal (`output.transcript_journal`, on by default) writes
  every utterance to `~/.local/state/voxtype/transcripts.jsonl` *before*
  delivery, and writes cancelled ones too. `voxtype transcript last|list|path`
  reads it back. This is the only reason a mis-delivered dictation is
  recoverable at all.
- The OSD text panel wrapped the *entire* transcript on every frame at 60 fps.
  Per-frame cost grew with everything said so far, which is invisible at the
  old 60s cap and a busy loop at half an hour. Now only the tail is wrapped.
- The generated Hyprland integration binds Escape and documents that a submap
  replaces the whole active bind set — upstream's `voxtype_recording` submap
  silently kills the user's own stop/toggle binding for the duration of the
  recording.

Local preferences, probably not wanted upstream as-is:

- The streaming Parakeet model is int8, quantized on this machine by
  `scripts/quantize-unified-int8.py` and registered as
  `parakeet-unified-en-0.6b-int8`. There is no artifact to download, so
  `download_parakeet_model` refuses to hit the mirror for it and prints the
  script instead. Same graph, same filenames, so `ParakeetUnifiedHandle::load`
  needs no code path of its own.

  This matters more than it sounds. `ParakeetUnified` is *buffered* streaming,
  not cache-aware: it re-encodes `left + chunk + right` seconds on every step,
  which NVIDIA's own model card admits ("The current inference pipeline
  supports only buffered streaming (left context is recomputed for each
  chunk)"), and the bobNight ONNX export makes it unavoidable — its encoder
  takes `audio_signal`/`length` and returns `outputs`/`encoded_lengths`, with
  no `cache_last_channel` tensors to carry state in. So the encoder runs for
  the whole recording and is the daemon's entire power budget.

  Measured here over 55.8s of speech (Core Ultra X7 358H, 16 cores, AVX-VNNI),
  in CPU-seconds per second of audio:

  | left/chunk/right | latency | fp32  | int8  |
  |------------------|---------|-------|-------|
  | 1.6/0.16/0.16    | 0.32s   | 2.27x | 1.30x |
  | 1.6/0.48/0.16    | 0.64s   | 0.83x | 0.48x |
  | 5.6/0.56/0.56    | 1.12s   | 1.84x | 0.97x |

  At 5.6/0.56/0.56 the int8 transcript was byte-identical to fp32 across 160
  words; elsewhere the only differences were punctuation. Resident set went
  from ~2.2 GB to ~0.89 GB.

- Streaming windows are 5.6 / 0.56 / 0.56, not the latency-chasing
  1.6 / 0.16 / 0.16 they were. That is NVIDIA's recommended operating point
  and exactly what VoiceInk runs (`FluidAudioModelManager.swift`: `leftFrames
  70, chunkFrames 7, rightFrames 7` — encoder frames at 80ms each, plus
  `parakeetUnifiedPrecision = .int8`). It is both cheaper and more accurate
  than what it replaced: left context is where the accuracy lives, short
  chunks are where the cost lives. 1.12s latency is the "1-2 seconds behind"
  that still reads as live.

- Preview-only streaming: partials render in the OSD rather than being typed
  at the cursor; the utterance is typed once on commit.
- `visual_mode = "text"` OSD, fed by a preview file (`src/osd/preview.rs`)
  that the daemon writes atomically and the OSD polls at 20Hz.
- Notch presentation: flush to the top edge, square corners, overlapping the
  compositor bar. Requires layer-shell exclusive zone `-1`; `0` respects other
  panels' reserved strips and lands the surface below the bar.
- The OSD maps its surface when the daemon publishes the preview file, not on
  the first audio frame, because opening the capture device costs ~0.6s.
- The OSD text panel carries an elapsed clock in its left gutter and renders
  a transient hint line (the armed-cancel prompt) published through a second
  runtime file, `hint.txt`, beside `preview.txt`.

## If you are rebasing onto upstream

The two bug fixes are the parts most likely to have been fixed upstream in the
meantime — check `src/config/engines/parakeet.rs` defaults and the
`top-center` arm in `src/bin/voxtype_osd_gtk4.rs` before reapplying them.
