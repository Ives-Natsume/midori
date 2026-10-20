# Third-party notices — midori-plugin-ssdv

This sidecar is licensed **GPL-3.0-or-later**, not the workspace's
MIT/Apache-2.0 default, because it links GPL crates from OpenHoshimi. It is a
separate process and the engine communicates with it over a pipe.

## OpenHoshimi

- <https://github.com/IzumiChino/OpenHoshimi>
- Copyright 2026 Izumi Chino, Mashiro Chen and Hyacinth Satellite Team
- License: GPL-3.0-or-later
- Pinned revision: `8cbd3f646731bcde8eb1b55b042ce9c74f225455`

OpenHoshimi's `Cargo.toml` and source SPDX headers declare
GPL-3.0-or-later; its top-level `LICENSE` file currently contains the GPLv2
text. This directory ships the GPLv3 text matching the declared license and
the SSDV reference implementation.

Used crates:

- `openhoshimi-soundmodem` — reentrant ASRTU-1 9k6 BPSK demodulator and the
  vendored HIT LilacSat Viterbi/RS/CCSDS FEC chain.
- `openhoshimi-codec` — `ssdv_jpeg::JpegBuilder`, which converts SSDV scan
  data into a baseline JPEG.

## SSDV

SSDV was designed by Philip Heron (M6PIZ). The packet format and JPEG
re-encoder follow the reference implementation:

- <https://github.com/fsphil/ssdv>
- License: GPL-3.0-or-later

The DSLWP 218-byte variant and its commonly used CRC initial value
(`0x4EE4FDE1`) follow the `ssdv_dslwp` fork used by ASRTU receivers. The
alternate JAMX initial value (`0x6AAAC1C5`) follows the same ASRTU/BY-04
practice. These values are **not selected by satellite name** in this
decoder: the user supplies the accepted initial value list, and the code only
contains the classic SSDV default (`0xFFFFFFFF`).

## ASRTU_Series_Receiver

The packet layout, virtual-channel selection, and the regression fixture come
from BG7ZDQ's MIT-licensed reference receiver:

- <https://github.com/BG7ZDQ/ASRTU_Series_Receiver>
- License: MIT

`tests/fixtures/asrtu1_ssdv_frames.b64` is derived from
`tests/fixtures/asrtu1_ssdv_capture.b64` in that repository (a real
ASRTU-1/AO-123 pass, image ID 43, 320x240).

## No vendored source

No third-party source is copied into this directory. Cargo resolves the
OpenHoshimi crates from the pinned Git revision, and the ASRTU fixture is test
data only.
