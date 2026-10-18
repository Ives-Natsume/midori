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
  |---------- stream ------------>|   (once per rate/frequency change)
  |---------- audio frames ------>|
  |<--------- events -------------|
  |---------- shutdown ---------->|
```

The plugin executable is described by a manifest. The engine builds the
manifest for built-in reference plugins and will eventually load them from
files for third-party plugins.

Example manifest:

```json
{
  "name": "cw",
  "version": "0.1.0",
  "protocol": 1,
  "command": ["midori-plugin-cw"],
  "input": { "type": "audio", "sample_rate_hz": 48000, "channels": 1 },
  "outputs": ["display", "packets"],
  "params": []
}
```

## Wire format

Every message has a 21-byte header:

```text
u32 payload_len (little-endian)
u8  kind
u64 seq (little-endian)
f64 timestamp_ns (little-endian)
payload
```

Kinds:

| kind | payload |
|---|---|
| 1 | JSON control/event |
| 2 | mono audio, f32 LE samples |
| 3 | soft bits / LLR bytes |
| 4 | framed or raw bytes |

Control messages are JSON objects with a `type` field:

- plugin → host: `hello`, `ready`, `display`, `status`, `error`, `packet`
- host → plugin: `configure`, `stream`, `shutdown`

`stream` is sent before the first data frame and whenever the sample rate,
centre frequency, or VFO changes:

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

The `doppler_hz` field is the correction currently applied to the VFO. The
plugin receives a stream that is already corrected; it may implement residual
AFC/tone tracking, but it must not compute SGP4 or retune hardware.

Raw IQ streams are not enabled for sidecars yet. When they are added they will
use shared memory; the current audio/byte path is pipe-friendly.

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

The engine/UI renders display events generically; plugins do not link egui.

## Reference plugin: CW

`plugins/midori-plugin-cw` mixes the audio to 700 Hz, tracks the envelope and
noise floor, estimates dot length adaptively, decodes Morse characters, and
emits text display events. It is intentionally small, but it exercises the
whole sidecar boundary.

```bash
cargo test -p midori-plugin-cw
cargo build --release --workspace
MIDORI_CW=1 MIDORI_AUTOCONNECT=1 ./target/release/midori-gui
```

## Writing a plugin

The minimal Rust implementation uses `midori-plugin`:

1. Read the `hello`/`configure`/`stream` messages from stdin.
2. Write `ready` after configuration.
3. Process `audio` frames.
4. Write JSON events with `kind = 1` frames.

Any language can implement the same framing. Keep the data plane binary and
the control plane JSON; do not send audio through JSON.
