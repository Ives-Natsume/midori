# Midori

A portable, decoder-centric SDR workbench for macOS and other platforms.

Midori is built around a small Rust core that accepts IQ from different
sources, runs a fixed DSP pipeline, records to SigMF, and hands audio or
packets to external decoder processes. The first target is amateur-satellite
and aviation-band work on a Mac: waterfall, VFO, AM/SSB/FM demodulation,
Doppler correction, recording, and plugins for SSTV/SSDV/FT4/CW.

> Status: early prototype. The core, CLI, and egui application live in
> `crates/`. The old Python audio loopback and the original `pluto_rs`
> prototype have been archived outside this repository.

## Current capabilities

- ADALM-PLUTO / AD9361 source through libiio (USB or network URI).
- RX1A (`A_BALANCED`) port selection, manual gain, sample rate and bandwidth.
- Internal `cf32` stream with sample-count timestamps.
- SigMF recording and replay (`ci16_le`, `cf32_le`).
- Averaged spectrum with peak reporting.
- AM channel demodulation (NCO + low-pass + envelope + DC block + resample)
  to 16-bit PCM WAV.
- FM channel demodulation (channelizer + polar discriminator + de-emphasis +
  audio low-pass + resample) to 16-bit PCM WAV.
- CW product demodulation with a 700 Hz BFO, feeding the CW decoder plugin.
- Live FM/AM/CW audio through `cpal`, driven by the same `VfoChannel` used for
  offline captures, with a simple AGC.
- egui desktop UI with a fixed three-column layout, stacked spectrum/waterfall,
  click-to-tune, a bottom CW/pass/log dock, demod selection, and one-click
  SigMF recording.
- Core contracts for frames, digital VFOs, channels, sinks, sessions, and
  out-of-process decoder plugins.
- Out-of-process sidecar host with a length-prefixed JSON/binary protocol.
- Reference CW plugin with adaptive tone detection and Morse decoding.
- Sidecar handshake/I/O timeouts and bounded queues so a hung or slow plugin
  cannot block the SDR worker.
- TLE parsing with checksums, Maidenhead grid conversion, SGP4 pass
  prediction, and Doppler range-rate calculation.
- Doppler metadata and `DopplerSource`/`TrackingConfig` contracts.
- Unit tests for the spectrum analyser, AM/FM demodulators, SigMF round-trip,
  plugin manifest parsing, sidecar framing, engine fan-out, and CW decoding.

## Build

Requirements:

- Rust 1.85+ (edition 2024).
- libiio with the `iio.framework` available on macOS, or libiio development
  files on Linux.
- A Pluto connected over USB for live capture.

```bash
cargo build --release
cargo test --workspace
```

On this development machine `iio.framework` is installed in
`/Library/Frameworks`. The local `.cargo/config.toml` adds the matching search
and rpath flags; it is intentionally gitignored. Start from
`.cargo/config.toml.example` if your framework lives there too. A distributable
build should bundle or statically link libiio instead.

## Quick start

Launch the desktop UI:

```bash
cargo run --release -p midori-app
# or: ./target/release/midori-gui
```

See the **UI** section below for the layout, satellite pass planner, and
development environment variables.

Record 30 seconds of the aviation band around 120 MHz from RX1A:

```bash
./target/release/midori record \
  --freq 120.0 --rate 2.4 --bw 2.4 --gain 40 \
  --seconds 30 --out captures/airband_120m
```

Inspect the capture and print the strongest bins:

```bash
./target/release/midori info captures/airband_120m
./target/release/midori spectrum captures/airband_120m --top 20
```

Demodulate an AM channel to audio:

```bash
./target/release/midori am captures/airband_120m \
  --freq 120.0 --bw 6000 --out captures/airband_120m.wav
```

Demodulate a broadcast FM channel (mono L+R):

```bash
./target/release/midori fm captures/fm_91.6 \
  --freq 91.6 --bw 200000 --deemph 50 \
  --out captures/fm_91.6.wav
```

For narrowband FM voice, use `--bw 15000 --audio-bw 3000` and optionally
`--deemph 0`.

Predict ISS passes for a Maidenhead locator:

```bash
./target/release/midori pass --tle captures/iss.tle \
  --grid PM95 --hours 24 --min-elev 5
```

`midori help` lists all options. Pass a capture base path; a trailing
`.sigmf` is accepted and normalised. The CLI adds the `.sigmf-data` /
`.sigmf-meta` suffixes.

## UI

The desktop UI uses a fixed three-column layout:

- **Left — Radio**: source, gain, demod summary, FFT/waterfall info, recording,
  satellite summary.
- **Centre — Spectrum + waterfall**: stacked view with a draggable splitter.
  Click either view to retune; the VFO and passband are overlaid.
- **Right — Decoders**: CW control, plugin list, decoder dock visibility.
- **Bottom dock**: `CW` output, `Passes` planner, and `Log`.

The Passes tab loads a TLE file, converts a Maidenhead locator, and predicts
the next 24 hours of passes with SGP4. The placeholder grid is `PM95`; pass
times are UTC. `MIDORI_TLE`, `MIDORI_CW`, and `MIDORI_AUTOCONNECT` are handy
for development.

The UI starts at 91.6 MHz FM with 60 dB gain for the current broadcast test,
and plays demodulated audio through the default output device. For the
airband, set the centre frequency to 120.0 MHz, choose AM, and reconnect to
apply. **Record IQ** writes a SigMF capture to `captures/gui_<timestamp>`.

Note: the Pluto/AD9361 accepts only certain sample rates. On the tested unit
`2.084`, `2.4`, `3.2`, and `4.0` MSPS work, while `1.0` and `2.0` MSPS are
rejected by the firmware. Use a supported rate and decimate digitally.

## Workspace layout

```
crates/
  midori-core/           # stream types, source trait, no hardware/UI deps
  midori-source-libiio/  # Pluto/AD9361 backend
  midori-dsp/            # spectrum, biquads, AM demodulator
  midori-sigmf/          # SigMF writer/reader
  midori-plugin/         # plugin manifest, events, host trait
  midori-engine/         # LiveSession, VFO channel, recorder sink
  midori-orbit/          # TLE, Maidenhead, SGP4 passes, Doppler
  midori-cli/            # `midori` command-line tool
  midori-app/            # egui desktop UI (`midori-gui`)
plugins/
  midori-plugin-cw/      # reference CW decoder sidecar
```

The old Python audio-loopback experiment and the original `pluto_rs`
prototype are stored as sibling directories outside the repository
(`../midori-audio-loopback/` and `../midori-pluto-rs-legacy/`).

## Design notes

- **Sources produce IQ; they do not know about DSP or UI.** A source returns
  `IqBlock`s containing an `Arc<[Complex32]>` plus `StreamSpec`, a
  sample index, and flags. This lets one block feed a waterfall, a recorder,
  and several decoder channels without copying.
- **Core owns contracts, not domain decoders.** `FrameMeta`, `Frame`,
  `VfoConfig`, `Channel`, `Sink`, `Session`, and the plugin manifest/host
  traits live in the core. SSTV, SSDV, FT4, APT, and CW logic do not.
- **Keep the hardware LO fixed during a pass.** Tuning and Doppler correction
  belong in a digital VFO/NCO, not in repeated PLL retunes. This keeps phase
  continuous and makes wideband recording straightforward.
- **Record SigMF from day one.** A capture is reproducible, testable without
  hardware, and usable by other tools. `midori:full_scale` preserves the
  Pluto's 12-bit scaling.
- **Decoders are separate processes.** FT4, SSDV, SSTV, and CW decoders have
  different languages, licenses, and failure modes. A sidecar supervisor with
  typed streams (`IQ`, `Audio`, `SoftBits`, `Bytes`) keeps the core clean.
  Plugins emit text/image/table display events rather than linking egui.
- **The core has no UI dependency.** The CLI and egui app are both consumers;
  a remote or Tauri client can use the same engine later.

## Plugins

Decoders run as separate processes. The host sends typed audio/soft-bit/byte
frames over stdin/stdout and receives JSON events (`packet`, `display`,
`status`, `error`) back. Plugins never link egui and never control hardware.

The CW plugin is the reference implementation:

```bash
cargo build --release --workspace
MIDORI_AUTOCONNECT=1 MIDORI_CW=1 ./target/release/midori-gui
```

It assumes a 700 Hz CW tone and writes decoded text into the CW panel. Enable
the **CW decoder** checkbox before connecting; changing it while connected
requires a reconnect. For real CW, select the **CW** demod, tune the hardware
LO to the carrier, and leave the VFO at zero offset; the core BFO places the
carrier at 700 Hz.

The plugin protocol and sidecar ABI are documented in
[`plugins/README.md`](plugins/README.md).

## Roadmap

1. Wire `Sgp4Doppler` into `LiveSession`/VFO and pass scheduling.
2. SSB/CW voice demodulation, squelch, and filter controls.
3. Click-to-tune digital VFOs and shareable presets.
4. APT/SSTV/SSDV plugins and plugin parameter UI.
5. SigMF browse/playback, packaging, and cross-platform audio.

## License

Intended to be MIT OR Apache-2.0. License files and the final choice will be
added before the first public release.
