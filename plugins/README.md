# Midori plugins

Decoder plugins run as separate processes and talk to the engine over
stdin/stdout. This keeps decoders language-independent, keeps their licenses
separate, and means a decoder crash cannot take down the SDR session.

## Lifecycle

```text
engine                          plugin
  |  spawn                        |
  |<--------- hello --------------|
  |---------- configure --------->|
  |<--------- ready --------------|
  |---------- stream ------------>|   (immediately on kind/rate/VFO changes;
  |                               |    frequency-only updates rate-limited)
  |---------- audio frames ------>|
  |<--------- events -------------|
  |---------- shutdown ---------->|
```

The plugin executable is described by a manifest. The reference plugins ship
`plugins/midori-plugin-*/manifest.json`; the engine discovers them at runtime
and still has a compiled-in fallback so a build tree without `plugins/` keeps
working.

Example manifest:

```json
{
  "name": "cw",
  "version": "0.1.0",
  "protocol": 2,
  "command": ["midori-plugin-cw"],
  "input": { "type": "audio", "sample_rate_hz": 48000, "channels": 1 },
  "outputs": ["display", "packets"],
  "demod_modes": ["cw"],
  "params": [
    { "name": "tone_hz", "kind": "float", "default": 700.0, "min": 100.0, "max": 3000.0 },
    { "name": "envelope_hz", "kind": "float", "default": 50.0, "min": 10.0, "max": 500.0 },
    { "name": "start_at_signal", "kind": "bool", "default": false }
  ]
}
```

### Discovery

Manifests are looked for in this order:

1. entries in `MIDORI_PLUGIN_PATH` (operating-system path-list separator);
2. `<directory of the midori executable>/plugins`;
3. a development `<workspace>/plugins`, found relative to the executable
   (`target/debug/midori-gui` → `../../plugins`).

Each directory is scanned for `*.json` manifests and `*/manifest.json` bundle
layouts. The first manifest with a given `name` wins. A malformed manifest is
reported and skipped; it does not hide the other plugins.

A relative `command[0]` is resolved against the manifest's directory first,
then the directory of the host executable, and finally left as a bare name for
the operating system's `PATH` lookup. `MIDORI_CW_PLUGIN`, `MIDORI_SSTV_PLUGIN`
and `MIDORI_SSDV_PLUGIN` still override the program for the reference decoders.

### Parameters and demod modes

`params` is the UI/configuration schema. Supported `kind` values are `bool`,
`int`, `float`, `string`, `enum` (with `values`), and `string_list` (an array
of strings). `default`, optional `min`/`max`, and `description` are used by the
host to build a complete parameter object; host overrides are rejected if they
name an unknown key, have the wrong type, fall outside the range, or select an
unknown enum value.

`demod_modes` lists the snake_case demodulator names whose frames make sense
for this plugin (`cw`; `sstv`/`fm`; `bpsk`/`bpsk_if`). A missing or empty list
means "any mode". When the VFO changes mode the worker pauses or resumes the
plugin and logs an `Info` event; paused plugins are not sent frames and those
frames are not counted as dropped.

## Wire format

Every message has a 21-byte header (protocol version 2):

```text
u32 payload_len (little-endian)
u8  kind
u64 seq (little-endian)
u64 timestamp_ns (little-endian, UTC nanoseconds)
payload
```

`timestamp_ns` is an exact unsigned nanosecond count. The sentinel
`0xFFFFFFFFFFFFFFFF` (`u64::MAX`) means "timestamp unknown"; the Rust API uses
`Option<u64>` and only applies the sentinel on the wire. Protocol v1 used an
`f64` here, which lost precision at epoch scale and could not distinguish an
unknown timestamp from zero.

Kinds:

| kind | payload |
|---|---|
| 1 | JSON control/event |
| 2 | mono audio, f32 LE samples |
| 3 | soft bits / LLR bytes |
| 4 | framed or raw bytes |
| 5 | interleaved complex IQ, f32 LE `(re, im)` pairs |

Control messages are JSON objects with a `type` field:

- plugin → host: `hello`, `ready`, `display`, `status`, `error`, `packet`
- host → plugin: `configure`, `stream`, `shutdown`

`stream` is sent before the first data frame. It is resent immediately when
the stream kind, sample rate, or VFO id changes. Live Doppler/AFC can move the
centre frequency on every block, so messages where only frequency metadata
(`center_freq_hz`, `nominal_freq_hz`, `doppler_hz`, `doppler_rate_hz_s`,
`bandwidth_hz`) changes are rate-limited to at most one per second:

```json
{
  "type": "stream",
  "kind": "audio",
  "sample_rate_hz": 48000,
  "channels": 1,
  "center_freq_hz": 145960000,
  "nominal_freq_hz": 145960000,
  "bandwidth_hz": 15000,
  "doppler_hz": 0,
  "doppler_rate_hz_s": 0,
  "vfo_id": 0
}
```

Plugins must tolerate repeated `stream` messages. Treat them as metadata
updates: do not reset decoder state unless `kind` or `sample_rate_hz` actually
changed. Frequency and Doppler fields can move during a single transmission.

The `doppler_hz` field is the correction currently applied to the VFO. The
plugin receives a stream that is already corrected; it may implement residual
AFC/tone tracking, but it must not compute SGP4 or retune hardware.

IQ sidecars are supported, but they receive the **channelized** complex output
of a digital VFO, not the multi-megasample source block. A plugin that declares
`{"type":"iq","sample_rate_hz":48000}` is only sent IQ frames carrying a
`vfo_id`; the core's VFO/Doppler stage has already selected and resampled the
signal. For 9k6 BPSK, add a 30 kHz BPSK channel and request 48 kHz. The data
plane is still a pipe (`kind = 5`, 8 bytes per sample); shared memory is only
needed if a future decoder asks for wideband IQ.

## Timeouts and backpressure

The host runs one reader thread and one writer thread per plugin, both with
bounded queues. The SDR worker never writes to a pipe directly.

- `hello` timeout: 3 s
- `ready` timeout: 3 s
- outbound queue: 32 frames; when full, the audio frame is dropped and the
  engine logs `[plugin] frame dropped: ...`
- shutdown wait: 1 s, then the child is killed

A plugin that hangs during handshake is killed and reported as an error. A
slow decoder cannot stall the receiver or the waterfall.

## Events

A plugin can emit:

- `display` with `text`, `image`, `table`, or `progress`
- `status` for WPM, SNR, lock state, etc.
- `packet` with a `FrameMeta` and bytes
- `error` with `fatal: bool`

Image updates carry dimensions so RGBA payloads can be uploaded directly:

```json
{
  "type": "display",
  "update": {
    "kind": "image",
    "title": "SSTV PD-120",
    "format": "png",
    "width": 640,
    "height": 496,
    "bytes": [137, 80, 78, 71]
  }
}
```

`format` is `png`, `jpeg`, or `rgba8`. PNG/JPEG carry their own dimensions;
`width`/`height` are still sent and are required for `rgba8`. Older plugins that
omit the fields deserialize with zero dimensions.

The engine/UI renders display events generically; plugins do not link egui.

## Reference plugin: CW

`plugins/midori-plugin-cw` mixes the audio near the configured BFO (700 Hz by
default), tracks small residual pitch errors, tracks the keying envelope with
an adaptive noise/signal floor, debounces short dropouts, merges fades inside a
dash, locks the dot timing after acquisition, and emits decoded text. It is
deliberately beacon-grade rather than contest-grade. Optional `configure`
parameters are `tone_hz`, `envelope_hz` (product-detector bandwidth; 50 Hz by
default, the GUI uses 120 Hz), and `start_at_signal` (the GUI sets this after
its leading-silence gate).

Decode an I/Q recording without a radio or TLE:

```bash
cargo run --release -p midori-plugin-cw --bin midori-cw-decode -- \
  data/BOTAN_TLM_CW_437393500Hz.wav --offset -13420
```

The offline tool supports RIFF and RF64 WAV, tracks the carrier drift directly
from the signal (no SGP4/TLE), demodulates to audio at a 700 Hz BFO, and can
optionally send the result through the real sidecar:

```bash
midori-cw-decode recording.wav --sidecar
```

The Botan (`JS1YPT`) regression fixture is a 12 kHz, 354 KiB mono WAV derived
from a 16.24 s RF64 capture. It decodes as:

```text
BOTAN JS1YPTNI8763
```

The short capture contains the callsign header and the start of the telemetry
field; the full 16-hex payload would need a longer recording or a repeat.

```bash
cargo test -p midori-plugin-cw
cargo test -p midori-plugin-cw --release --test botan -- --ignored
```

The ignored test runs the complete RF64 → tracking → audio → decode pipeline
against the local `data/BOTAN_TLM_CW_437393500Hz.wav` file. The 112 MB raw
capture is gitignored; the small derived fixture is committed.

## Reference plugin: SSTV

`plugins/midori-plugin-sstv` wraps the pure-Rust
[slowrx](https://github.com/jasonherald/slowrx.rs) decoder. It accepts mono
audio at the device rate (48 kHz by default), detects VIS automatically,
reports progress per image line, and emits a PNG when an image completes.
Completed images are also written to `captures/` by the sidecar.

Primary test modes are **PD-120** (ARISS) and **Robot 36**. The underlying
library also supports PD-180/240, Robot 24/72, Martin 1/2, and Scottie 1/2/DX;
Scottie and Martin have no real-radio reference corpus upstream yet.

Run the fast tests (VIS detection and PNG encoding):

```bash
cargo test -p midori-plugin-sstv
```

Full-image decodes are CPU-heavy in debug builds and are marked ignored. Run
them in release:

```bash
cargo test -p midori-plugin-sstv --release -- --ignored
```

Decode a WAV without hardware:

```bash
cargo run --release -p midori-plugin-sstv --bin midori-sstv-decode -- \
  recording.wav --out-dir captures
```

## Reference plugin: SSDV

`plugins/midori-plugin-ssdv` decodes SSDV image streams. It parses 223-byte
CCSDS frames, 218-byte DSLWP packets, and classic 256-byte packets, reassembles
each image with gap filling, and emits PNG display events. For the 9k6
BPSK/CCSDS link it can take the BPSK channel's 48 kHz complex baseband and run
OpenHoshimi's Viterbi / RS(255,223) soundmodem (validated with ASRTU-1); the
SSDV layer itself is link-agnostic. It also writes the JPEG,
PNG, and a per-image concatenation of received packets to `captures/`.

The SSDV CRC32 polynomial (`0xEDB88320`) and final XOR are fixed, but the
initial value differs between implementations. It is supplied by the caller,
not selected by a satellite name: classic SSDV uses `0xFFFFFFFF`, ASRTU-1 /
DSLWP uses `0x4EE4FDE1`, and some JAMX/BY-04 streams use `0x6AAAC1C5`.
Multiple values can be supplied and are tried in order. The 223-byte CCSDS
frame header length is also configurable (default 5 bytes). The offline tool
also handles mono 48 kHz, 12 kHz real-IF audio through the soundmodem's `i16`
input; the live sidecar uses the lossless IQ path.

```bash
MIDORI_AUTOCONNECT=1 MIDORI_SSDV=1 ./target/release/midori-gui
```

Select **BPSK** for zero-IF I/Q, or **BPSK IF** for a mono 48 kHz / 12 kHz
real-IF recording, and enable **SSDV decoder**. Set the stream's CRC32 initial
value(s) and frame header length in the SSDV panel; these are not selected by
satellite name. The VFO must sit on the image downlink
(436.210 MHz for the ASRTU-1 example). The sidecar accepts raw input for
offline work:

```bash
cargo run --release -p midori-plugin-ssdv --bin midori-ssdv-decode -- \
  recording.wav --offset 750000 --crc-init 0x4EE4FDE1 --out-dir captures

cargo run --release -p midori-plugin-ssdv --bin midori-ssdv-decode -- \
  ASRTU1_IF.wav --audio-if --crc-init 0x4EE4FDE1 --out-dir captures

cargo run --release -p midori-plugin-ssdv --bin midori-ssdv-decode -- \
  asrtu1_frames.b64 --crc-init 0x4EE4FDE1 --out-dir captures
```

The raw path accepts 223-byte CCSDS frames, 218-byte DSLWP packets, 256-byte
classic SSDV packets, or a base64 dump of any of these. The regression fixture
is a real ASRTU-1 pass from BG7ZDQ/ASRTU_Series_Receiver (MIT). Local ignored
tests also cover a full ASRTU-1 zero-IF I/Q pass and a BY-04 12 kHz-IF
recording with both CRC initial values and 640x480 images:

```bash
cargo test -p midori-plugin-ssdv --release --test asrtu_live -- --ignored
```

The sidecar links GPL-3.0-or-later OpenHoshimi crates and therefore is
GPL-3.0-or-later itself. It is deliberately out of process so the rest of
Midori can remain MIT/Apache-2.0. See `midori-plugin-ssdv/NOTICE.md`.

## Writing a plugin

Rust plugins should use `midori-plugin-sdk`. It owns the handshake, control
loop, frame decoding, error reporting, and shutdown flush so the plugin only
contains decoder logic:

```rust
use anyhow::Result;
use midori_plugin_sdk::{
    Emitter, FrameInfo, Plugin, StatusLevel, StreamInfo, run,
};

struct Tones {
    sample_rate_hz: f64,
}

impl Plugin for Tones {
    fn name(&self) -> &str {
        "tones"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn configure(&mut self, params: &serde_json::Value) -> Result<()> {
        let _ = params; // Read tone_hz, output_dir, ... here.
        Ok(())
    }

    fn on_stream(&mut self, info: &StreamInfo, _out: &mut Emitter<'_>) -> Result<()> {
        // Repeated stream messages are metadata updates. Reset decoder state
        // only when info.kind or info.sample_rate_hz actually changed.
        self.sample_rate_hz = info.sample_rate_hz;
        Ok(())
    }

    fn on_audio(
        &mut self,
        meta: &FrameInfo,
        samples: &[f32],
        out: &mut Emitter<'_>,
    ) -> Result<()> {
        let _ = (meta.seq, meta.timestamp_ns, samples);
        out.status(StatusLevel::Info, "decoding")?;
        // out.text("Tones", vec!["...".to_string()])?;
        Ok(())
    }

    fn on_shutdown(&mut self, out: &mut Emitter<'_>) -> Result<()> {
        // Flush partial results. The SDK calls this exactly once on
        // `shutdown` or stdin EOF.
        let _ = out;
        Ok(())
    }
}

fn main() -> Result<()> {
    run(Tones {
        sample_rate_hz: 48_000.0,
    })
}
```

The crate dependencies are `midori-plugin-sdk`, `anyhow`, and `serde_json`
(the SDK re-exports the event and stream types). Override `on_iq`,
`on_bytes`, and `on_soft_bits` for the other data planes. `configure` errors
are returned to the host as an `error` control message and the SDK waits for
another `configure`; errors returned by frame handlers become non-fatal
events and processing continues.

The protocol underneath is language-neutral:

1. Send `hello` with `protocol: 2` on stdout, then read `configure` and
   `stream` messages from stdin.
2. Write `ready` after configuration. If configuration fails, write
   `{"type":"error","message":"..."}` and wait for another `configure` or
   exit.
3. Process `audio`, `iq`, soft-bit, or byte frames declared by the manifest.
   Frames carry an exact `u64` UTC timestamp or the "unknown" sentinel.
   Frames received before a successful `configure` are ignored.
4. Write JSON events with `kind = 1` frames.
5. On `shutdown` or stdin EOF, flush any buffered final output, then exit.

Treat `stream` messages as idempotent metadata updates: repeated messages and
frequency-only changes must not reset the decoder. Only `kind` or
`sample_rate_hz` changes require reinitialising decoder state.

Keep the data plane binary and the control plane JSON; do not send audio
through JSON.
