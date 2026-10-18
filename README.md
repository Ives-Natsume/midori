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
- egui desktop UI with spectrum/waterfall, frequency/gain controls, and
  one-click SigMF recording.
- Unit tests for the spectrum analyser, AM demodulator, and SigMF round-trip.

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

The UI currently connects to RX1A at 2.4 MSPS with manual gain. Set the centre
frequency and gain, press **Connect Pluto**, then switch between spectrum and
waterfall. **Record IQ** writes a SigMF capture to `captures/gui_<timestamp>`.
For development, `MIDORI_AUTOCONNECT=1` connects on startup.

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

`midori help` lists all options. Pass a capture base path; a trailing
`.sigmf` is accepted and normalised. The CLI adds the `.sigmf-data` /
`.sigmf-meta` suffixes.

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
  midori-cli/            # `midori` command-line tool
  midori-app/            # egui desktop UI (`midori-gui`)
```

The old Python audio-loopback experiment and the original `pluto_rs`
prototype are stored as sibling directories outside the repository
(`../midori-audio-loopback/` and `../midori-pluto-rs-legacy/`).

## Design notes

- **Sources produce IQ; they do not know about DSP or UI.** A source returns
  `IqBlock`s containing an `Arc<[Complex32]>` plus `StreamSpec`, a
  sample index, and flags. This lets one block feed a waterfall, a recorder,
  and several decoder channels without copying.
- **Keep the hardware LO fixed during a pass.** Tuning and Doppler correction
  belong in a digital VFO/NCO, not in repeated PLL retunes. This keeps phase
  continuous and makes wideband recording straightforward.
- **Record SigMF from day one.** A capture is reproducible, testable without
  hardware, and usable by other tools. `midori:full_scale` preserves the
  Pluto's 12-bit scaling.
- **Decoders are separate processes.** FT4, SSDV, SSTV, and CW decoders have
  different languages, licenses, and failure modes. A sidecar supervisor with
  typed streams (`IQ`, `Audio`, `SoftBits`, `Bytes`) keeps the core clean.
- **The core has no UI dependency.** The CLI and egui app are both consumers;
  a remote or Tauri client can use the same engine later.

## Roadmap

1. File source and a streaming session/ring-buffer layer.
2. SSB/CW/FM demodulation, AGC, squelch, and live audio output (`cpal`).
3. Decoder sidecar protocol and the first plugin (SSDV or FT4).
4. TLE/SGP4 Doppler tracking, pass scheduling, and presets.
5. Live audio/waterfall polish, SigMF browse/playback, and packaging.

## License

Intended to be MIT OR Apache-2.0. License files and the final choice will be
added before the first public release.
