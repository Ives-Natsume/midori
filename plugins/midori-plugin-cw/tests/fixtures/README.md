# CW test fixtures

## `botan_cw_12k.wav`

- Source: `data/BOTAN_TLM_CW_437393500Hz.wav` (not committed; 112 MB RF64,
  1.8 MSPS, 16.24 s).
- Content: Botan (`JS1YPT`) CW telemetry around 437.380 MHz, roughly 20 WPM.
- Processing: coarse offset `-13420 Hz`, signal-derived carrier tracking,
  trimming to the first strong burst, and 48 kHz → 12 kHz resampling.
- Format: mono, 12 kHz, 16-bit PCM, 15.09 s, 354 KiB.
- Expected decode: `BOTAN JS1YPTNI8763` (the capture ends before the complete
  16-hex telemetry field).
- No TLE or SGP4 was used.

Regenerate with:

```bash
cargo run --release -p midori-plugin-cw --bin midori-cw-decode -- \
  data/BOTAN_TLM_CW_437393500Hz.wav \
  --offset -13420 --rate 12000 \
  --wav-out plugins/midori-plugin-cw/tests/fixtures/botan_cw_12k.wav
```
