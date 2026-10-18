# Third-party notices — midori-plugin-sstv

This sidecar uses the `slowrx` crate (MIT):

- <https://github.com/jasonherald/slowrx.rs>
- <https://crates.io/crates/slowrx>

`slowrx.rs` is a pure-Rust port of **slowrx** by Oona Räisänen (OH2EIQ),
distributed under the ISC license:

- <https://github.com/windytan/slowrx>

The tests use `rasterwave` (MIT) only to generate synthetic SSTV audio:

- <https://github.com/boybook/rasterwave.rs>

No third-party code is vendored in this repository; dependencies are resolved
by Cargo.
