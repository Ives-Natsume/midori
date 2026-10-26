# Midori

A portable, decoder-centric SDR workbench for macOS and other platforms.

Midori is built around a small Rust core that accepts IQ from different
sources, runs a fixed DSP pipeline, records to SigMF, and hands audio or
packets to external decoder processes. The first target is amateur-satellite
and aviation-band work on a Mac: waterfall, VFO, AM/FM/CW demodulation,
Doppler correction, recording, and plugins for SSTV/SSDV/FT4/CW.

> Status: early prototype. The core, CLI, and egui application live in
> `crates/`. The old Python audio loopback is archived outside this
> repository; the original nested `pluto_rs` prototype was removed.
>
## Current capabilities

- ADALM-PLUTO / AD9361 source through libiio (USB or network URI).
- RIFF/RF64 I/Q WAV playback in the GUI, paced to real time, with digital VFO
  tuning inside the recorded band.
- RX1A (`A_BALANCED`) port selection, manual gain, sample rate and bandwidth.
- Internal `cf32` stream with sample-count timestamps.
- SigMF recording and replay (`ci16_le`, `cf32_le`).
- Averaged spectrum with peak reporting.
- AM channel demodulation (NCO + low-pass + envelope + DC block + resample)
  to 16-bit PCM WAV.
- FM channel demodulation (channelizer + polar discriminator + de-emphasis +
  audio low-pass + resample) to 16-bit PCM WAV.
- CW product demodulation with a 700 Hz BFO, feeding the CW decoder plugin,
  plus a real-time carrier AFC and a 4 s band scan for auto-tuning.
- CW sidecar with adaptive tone/timing, fade-tolerant keying, leading-silence
  gating, and offline I/Q WAV decoding (`midori-cw-decode`) with
  signal-derived carrier tracking; validated against a Botan `JS1YPT` telemetry
  capture without a TLE.
- SSTV decoding sidecar (slowrx) for PD-120, PD-180, Robot 36/72, Robot 24,
  Martin 1/2, and Scottie 1/2/DX; images are shown in the SSTV tab and saved as
  PNG. PD-120 and Robot 36 are the primary validated modes.
- Offline SSTV decoding from WAV (`midori-sstv-decode`) for testing without a
  transmitter.
- Complex BPSK channel mode (30 kHz in, 48 kHz IQ out) and a **BPSK IF**
  pass-through mode for mono 48 kHz / 12 kHz real-IF recordings, with the same
  digital VFO and SGP4 Doppler path as the audio modes.
- Generic SSDV sidecar: OpenHoshimi's 9k6 BPSK/CCSDS soundmodem, DSLWP
  218-byte and classic 256-byte SSDV packets, image reassembly with gap
  filling, JPEG/PNG output, and a preview in the SSDV tab. The CRC32 initial
  value and CCSDS frame header length are supplied by the user rather than
  baked in per satellite.
- Offline SSDV decoding from I/Q WAV, 48 kHz mono 12 kHz-IF audio, or raw
  packet/frame dumps (`midori-ssdv-decode`), validated against real ASRTU-1
  and BY-04 passes.
- Live FM/AM/CW audio through `cpal`, driven by the same `VfoChannel` used for
  offline captures, with a simple AGC.
- Demod mode metadata (labels, config keys, implementation status, output
  kind) lives in `midori-core`; the engine, config, and UI derive from it.
  USB/LSB remain reserved in the config schema but are not implemented or
  selectable.
- egui desktop UI with a fixed three-column layout, stacked spectrum/waterfall,
  click-to-tune, per-mode RF bandwidth controls, a waterfall that can follow or
  independently show the spectrum zoom, a stable dB scale, a bottom
  CW/pass/log dock, demod selection, and one-click SigMF recording.
- Core contracts for frames, digital VFOs, channels, sinks, sessions, and
  out-of-process decoder plugins.
- Out-of-process sidecar host with a length-prefixed JSON/binary protocol.
- Reference CW plugin with adaptive tone detection and Morse decoding.
- Sidecar handshake/I/O timeouts and bounded queues so a hung or slow plugin
  cannot block the SDR worker.
- TLE parsing with checksums, Maidenhead grid conversion, SGP4 pass
  prediction, and Doppler range-rate calculation.
- Live SGP4 Doppler correction applied to the digital VFO (single satellite,
  armed before connecting), with offset/rate shown in the UI.
- TOML configuration with compiled fallbacks, manual save, and reload.
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

Decode an ASRTU-1 SSDV recording (9k6 BPSK). For a zero-IF I/Q capture, give
the signal offset inside the recording:

```bash
./target/release/midori-ssdv-decode recording.wav --offset 750000 --out-dir captures
```

For the reference receiver's mono 48 kHz, 12 kHz real-IF recordings:

```bash
./target/release/midori-ssdv-decode ASRTU1_IF.wav --audio-if --out-dir captures
```

Decode a raw dump of 223-byte CCSDS frames, 218-byte DSLWP packets, or
256-byte classic SSDV packets:

```bash
./target/release/midori-ssdv-decode asrtu1_frames.b64 --out-dir captures
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

- **Left — Radio**: source input (Pluto or I/Q file), gain, demod summary,
  FFT/waterfall info, recording, satellite summary.
- **Centre — Spectrum + waterfall**: stacked view with a draggable splitter.
  Click either view to retune; the VFO and passband are overlaid. The
  spectrum dB scale is fixed by the **FFT / Waterfall** dB-min/max controls, so
  replaying a recording does not make the y axis jump. **Waterfall follows
  spectrum zoom** crops the waterfall to the spectrum view; when it is off the
  full band is shown and yellow boundary lines mark the spectrum view.
- **Right — Decoders**: CW enable, AFC and **Auto tune CW carrier** (with a
  warning when no narrowband CW signal is found), SSTV and SSDV controls
  (including the SSDV CRC initial value and CCSDS frame header length),
  plugin list, decoder dock visibility.
- **Left — Demod**: mode buttons come from the modes implemented by the
  engine (FM, AM, CW, SSTV, BPSK, BPSK IF); editable RF bandwidth, audio
  bandwidth, and FM de-emphasis; **BPSK** selects the complex 48 kHz data
  channel, and **BPSK IF** passes a mono 48 kHz / 12 kHz real-IF recording
  straight to the data decoder. Changes are applied to the running session
  after a short debounce (no reconnect needed).
- **Bottom dock**: `CW`, `SSTV`, `SSDV`, `Passes` planner, and `Log`. The
  SSTV and SSDV tabs split vertically: status/controls on the left, decoded
  image on the right.

For an I/Q file, choose **Input → I/Q file**, enter a RIFF/RF64 WAV path, and
press **From filename** to take the centre frequency from a name like
`..._437393500Hz.wav`. **Start file** plays in real time. Clicking the spectrum
moves the digital VFO inside the recording; the file centre/LO stays fixed.
Use `MIDORI_FILE`, `MIDORI_VFO`, `MIDORI_AUTOCONNECT`, and `MIDORI_CW` for a
hands-free replay test:

```bash
MIDORI_FILE=data/BOTAN_TLM_CW_437393500Hz.wav MIDORI_VFO=-13420 \
  MIDORI_AUTOCONNECT=1 MIDORI_CW=1 ./target/release/midori-gui
```

The Passes tab loads a TLE file, converts a Maidenhead locator, and predicts
the next 24 hours of passes with SGP4. The placeholder grid is `PM95`; pass
times are UTC. `MIDORI_CW`, `MIDORI_DOPPLER`, and `MIDORI_AUTOCONNECT` are handy
for development.

Enable **Track Doppler (SGP4)** in the Satellite panel before connecting to
apply range-rate correction to the digital VFO. Keep the hardware LO on the
nominal downlink; the VFO offset and rate are shown in the receiver strip.

## Configuration

Settings live in `config.toml` in the working directory during development.
The file is gitignored and may contain local frequencies, grid locators, and
TLE paths; `config.example.toml` is the committed template.

- Loaded once at startup, not every frame.
- Missing sections/fields use compiled fallbacks.
- A missing file starts from neutral defaults (100 MHz, FM, 40 dB, empty grid).
- A malformed file falls back to defaults and reports a warning in the Log tab;
  the bad file is not overwritten.
- Settings are written only when **Save settings** is clicked. There is no
  autosave.
- **Reload config** re-reads the file without restarting the app. Source/demod
  changes still require a reconnect.

To start in file mode from config:

```toml
[source]
kind = "file"            # "pluto" or "file"
file_path = "data/BOTAN_TLM_CW_437393500Hz.wav"
file_loop = false
center_freq_hz = 437_393_500
```

`center_freq_hz` is the recording centre for a file, not the VFO frequency.
`MIDORI_FILE` and `MIDORI_VFO` are development shortcuts for the same thing.

A platform-specific config directory will be added later. For now the file is
in the workspace so it is easy to inspect while developing.

The compiled fallback starts at 100.0 MHz FM with 40 dB gain. The development
`config.toml` currently sets 91.6 MHz / 60 dB for broadcast testing; it is
gitignored. For the airband, set 120.0 MHz, choose AM, and reconnect to apply.
**Record IQ** writes a SigMF capture to `captures/gui_<timestamp>`.

Note: the Pluto/AD9361 accepts only certain sample rates. On the tested unit
`2.084`, `2.4`, `3.2`, and `4.0` MSPS work, while `1.0` and `2.0` MSPS are
rejected by the firmware. Use a supported rate and decimate digitally.

## Workspace layout

```
crates/
  midori-core/           # stream types, source trait, no hardware/UI deps
  midori-source-libiio/  # Pluto/AD9361 backend
  midori-source-file/    # RIFF/RF64 I/Q WAV playback
  midori-dsp/            # spectrum, biquads, AM demodulator
  midori-sigmf/          # SigMF writer/reader
  midori-plugin/         # plugin manifest, events, host trait
  midori-engine/         # LiveSession, VFO channel, recorder sink
  midori-orbit/          # TLE, Maidenhead, SGP4 passes, Doppler
  midori-config/         # TOML settings, fallbacks, atomic save
  midori-cli/            # `midori` command-line tool
  midori-app/            # egui desktop UI (`midori-gui`)
plugins/
  midori-plugin-cw/      # reference CW decoder sidecar
  midori-plugin-sstv/    # SSTV sidecar + offline WAV decoder
  midori-plugin-ssdv/    # ASRTU-1 SSDV sidecar + offline decoder (GPL)
```

The old Python audio-loopback experiment is archived at
`../midori-audio-loopback/` outside this repository. The original nested
`pluto_rs` prototype and its Git metadata were removed.

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
  IQ sidecars receive a channelized 48 kHz complex stream, not the wideband
  source, so the VFO and Doppler correction still apply.
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

It writes decoded text into the CW panel. Enable the **CW decoder** checkbox
before connecting; changing it while connected requires a reconnect. For real
CW, select the **CW** demod and either tune near the carrier and let **AFC**
pull it in, or press **Auto tune CW carrier** to scan the band for a narrowband
peak. The AFC keeps a drifting satellite carrier on the 700 Hz BFO, and the
plugin also tracks small residual pitch errors. The default CW channel is
2 kHz wide, which is more tolerant of drift than the old 500 Hz setting; the
bandwidth is editable in the Demod panel.

For a recording, the offline CW tool reads RIFF/RF64 I/Q WAV, tracks the
carrier drift itself (no TLE needed), and can exercise the real sidecar:

```bash
./target/release/midori-cw-decode recording.wav --offset -13420
./target/release/midori-cw-decode recording.wav --sidecar
```

The Botan (`JS1YPT`) test capture decodes as `BOTAN JS1YPTNI8763`; the raw
112 MB capture is gitignored and a small 12 kHz fixture is used in CI.

The SSTV sidecar uses [slowrx](https://github.com/jasonherald/slowrx.rs):

```bash
MIDORI_AUTOCONNECT=1 MIDORI_SSTV=1 ./target/release/midori-gui
```

Select the **SSTV** demod (FM, 15 kHz channel, 3 kHz audio, no de-emphasis),
enable the **SSTV decoder**, and tune an FM SSTV signal. VIS is detected
automatically. PD-120 and Robot 36 are the primary test modes; PD-180 and
Martin 1/2 are also covered by the offline tests. Images appear in the SSTV
tab and are written to `captures/sstv_<timestamp>_<mode>.png`.

Without a transmitter or radio, decode a WAV recording offline:

```bash
./target/release/midori-sstv-decode recording.wav --out-dir captures
```

The plugin protocol and sidecar ABI are documented in
[`plugins/README.md`](plugins/README.md).

### SSDV

`plugins/midori-plugin-ssdv` decodes SSDV image streams. It accepts 223-byte
CCSDS frames, 218-byte DSLWP packets, and classic 256-byte SSDV packets. For
the 9k6 BPSK/CCSDS link it can take the **BPSK** channel's 48 kHz complex
baseband and run OpenHoshimi's Viterbi/RS(255,223) soundmodem (validated with
ASRTU-1). It then reassembles the JPEG scan data, filling missing packets, and
emits PNG image events. The SSDV layer itself is link-agnostic.

Select **BPSK** for a zero-IF I/Q recording, or **BPSK IF** for a mono 48 kHz
recording with the signal at a 12 kHz intermediate frequency. Then tune the
image downlink and enable **SSDV decoder**. Set the stream's CRC32 initial value(s)
in the SSDV panel; for ASRTU-1/DSLWP that is `0x4EE4FDE1`, while classic SSDV
uses `0xFFFFFFFF`. The polynomial is fixed by SSDV; only the initial value
differs between implementations. The 223-byte frame header length defaults to
5 bytes. Images,
JPEGs, and per-image packet dumps are written to `captures/`.

```bash
MIDORI_AUTOCONNECT=1 MIDORI_SSDV=1 ./target/release/midori-gui
```

Decode without hardware:

```bash
# Zero-IF I/Q WAV, SSDV signal at +750 kHz
./target/release/midori-ssdv-decode recording.wav --offset 750000 \
  --crc-init 0x4EE4FDE1

# Mono 48 kHz, 12 kHz real-IF recording
./target/release/midori-ssdv-decode ASRTU1_IF.wav --audio-if \
  --crc-init 0x4EE4FDE1

# Raw 223-byte CCSDS frames (or 218-byte DSLWP / 256-byte SSDV packets)
./target/release/midori-ssdv-decode asrtu1_frames.b64 --crc-init 0x4EE4FDE1
```

The fixture in `plugins/midori-plugin-ssdv/tests/fixtures/` is a real
ASRTU-1 pass (image ID 43, 320x240) from
[BG7ZDQ/ASRTU_Series_Receiver](https://github.com/BG7ZDQ/ASRTU_Series_Receiver).
The decoder has also been checked against local ASRTU-1 zero-IF I/Q and
BY-04 12 kHz-IF captures. BY-04 accepts both `0x4EE4FDE1` and the alternate
`0x6AAAC1C5` initial value; pass both to `--crc-init` if needed. Those large
recordings are not committed; the ignored tests are:

```bash
cargo test -p midori-plugin-ssdv --release --test asrtu_live -- --ignored
```

This sidecar links the GPL-3.0-or-later OpenHoshimi crates for its
soundmodem and JPEG assembler, so the sidecar itself is GPL-3.0-or-later and
stays out of process. The rest of Midori remains MIT/Apache-2.0. See
`plugins/midori-plugin-ssdv/NOTICE.md`.

## Roadmap

1. SSTV live preview, real ARISS regression recordings, then APT.
2. Pass Arm/AOS/LOS automation and automatic Doppler arming.
3. SSB/CW voice demodulation, squelch, and filter presets.
4. Shareable receiver presets and SigMF browse/playback.
5. Packaging and cross-platform audio.

## License

The core and the CW/SSTV plugins are intended to be MIT OR Apache-2.0.
`plugins/midori-plugin-ssdv` is GPL-3.0-or-later because it uses the GPL
OpenHoshimi soundmodem and SSDV JPEG assembler; it runs as a separate
process. License files and the final choice for the MIT/Apache parts will be
added before the first public release.
