//! libiio backend for ADALM-PLUTO and compatible AD9361 devices.
//!
//! This is the first hardware backend. It is deliberately small and uses the
//! libiio C API directly; a SoapySDR backend can be added later without
//! touching the rest of the engine.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use midori_core::{
    GainMode, IqBlock, IqSource, SampleFormat, SourceCaps, SourceConfig, StreamSpec,
};
use num_complex::Complex32;

const DEFAULT_URI: &str = "usb:";

struct ChannelFormat {
    bits: u32,
    shift: u32,
}

impl ChannelFormat {
    fn full_scale(&self) -> f64 {
        (1u64 << self.bits.saturating_sub(1).min(30)) as f64
    }
}

/// A Pluto/AD9361 source backed by libiio.
pub struct LibiioSource {
    ctx: *mut libiio_sys::iio_context,
    phy_dev: *mut libiio_sys::iio_device,
    rx_dev: *mut libiio_sys::iio_device,
    buffer: *mut libiio_sys::iio_buffer,

    config: Option<SourceConfig>,
    spec: Option<StreamSpec>,
    sample_step: usize,
    start_sample: u64,
    first_timestamp_ns: Option<u64>,

    i_format: Option<ChannelFormat>,
    q_format: Option<ChannelFormat>,
}

// The raw libiio pointers are owned by this object and libiio is used from a
// single worker thread at a time.
unsafe impl Send for LibiioSource {}

impl LibiioSource {
    /// Connect to a libiio context. `uri` is usually `usb:` or `ip:<address>`.
    pub fn open(uri: Option<&str>) -> Result<Self> {
        let uri = uri.unwrap_or(DEFAULT_URI);
        let uri_c = CString::new(uri).context("invalid libiio URI")?;

        let ctx = unsafe { libiio_sys::iio_create_context_from_uri(uri_c.as_ptr()) };
        if ctx.is_null() {
            bail!("could not create libiio context for URI '{uri}'");
        }

        let phy_dev = unsafe {
            libiio_sys::iio_context_find_device(ctx, CString::new("ad9361-phy")?.as_ptr())
        };
        let rx_dev = unsafe {
            libiio_sys::iio_context_find_device(ctx, CString::new("cf-ad9361-lpc")?.as_ptr())
        };

        if phy_dev.is_null() || rx_dev.is_null() {
            unsafe { libiio_sys::iio_context_destroy(ctx) };
            bail!("libiio context has no ad9361-phy / cf-ad9361-lpc devices");
        }

        Ok(Self {
            ctx,
            phy_dev,
            rx_dev,
            buffer: ptr::null_mut(),
            config: None,
            spec: None,
            sample_step: 4,
            start_sample: 0,
            first_timestamp_ns: None,
            i_format: None,
            q_format: None,
        })
    }

    fn phy_channel(&self, name: &str, output: bool) -> Result<*mut libiio_sys::iio_channel> {
        let name_c = CString::new(name)?;
        let chan =
            unsafe { libiio_sys::iio_device_find_channel(self.phy_dev, name_c.as_ptr(), output) };
        if chan.is_null() {
            bail!(
                "channel '{name}' ({}) not found",
                if output { "output" } else { "input" }
            );
        }
        Ok(chan)
    }

    fn rx_channel(&self, name: &str) -> Result<*mut libiio_sys::iio_channel> {
        let name_c = CString::new(name)?;
        let chan =
            unsafe { libiio_sys::iio_device_find_channel(self.rx_dev, name_c.as_ptr(), false) };
        if chan.is_null() {
            bail!("RX data channel '{name}' not found");
        }
        Ok(chan)
    }

    fn write_attr(chan: *mut libiio_sys::iio_channel, attr: &str, value: &str) -> Result<()> {
        let attr = CString::new(attr)?;
        let value = CString::new(value)?;
        let ret =
            unsafe { libiio_sys::iio_channel_attr_write(chan, attr.as_ptr(), value.as_ptr()) };
        if ret < 0 {
            bail!("failed to write attribute (error {ret})");
        }
        Ok(())
    }

    fn read_attr(chan: *mut libiio_sys::iio_channel, attr: &str) -> Result<String> {
        let attr = CString::new(attr)?;
        let mut buf = [0 as c_char; 256];
        let ret = unsafe {
            libiio_sys::iio_channel_attr_read(chan, attr.as_ptr(), buf.as_mut_ptr(), buf.len())
        };
        if ret < 0 {
            bail!("failed to read attribute (error {ret})");
        }
        let value = unsafe { CStr::from_ptr(buf.as_ptr()) };
        Ok(value.to_string_lossy().into_owned())
    }

    fn read_f64_attr(chan: *mut libiio_sys::iio_channel, attr: &str) -> Option<f64> {
        Self::read_attr(chan, attr)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
    }

    fn set_rx_lo(&self, hz: f64) -> Result<f64> {
        let chan = self.phy_channel("altvoltage0", true)?;
        Self::write_attr(chan, "frequency", &format!("{:.0}", hz))?;
        Ok(Self::read_f64_attr(chan, "frequency").unwrap_or(hz))
    }

    fn data_format(&self, chan: *mut libiio_sys::iio_channel) -> Result<ChannelFormat> {
        let format = unsafe { libiio_sys::iio_channel_get_data_format(chan) };
        if format.is_null() {
            bail!("channel has no data format");
        }
        let format = unsafe { &*format };
        if format.length != 16 {
            bail!(
                "unsupported sample word length {} (expected 16-bit IQ)",
                format.length
            );
        }
        if format.is_be {
            bail!("big-endian IQ data is not supported yet");
        }
        Ok(ChannelFormat {
            bits: format.bits,
            shift: format.shift,
        })
    }
}

impl Drop for LibiioSource {
    fn drop(&mut self) {
        let _ = self.stop();
        unsafe {
            if !self.ctx.is_null() {
                libiio_sys::iio_context_destroy(self.ctx);
                self.ctx = ptr::null_mut();
            }
        }
    }
}

impl IqSource for LibiioSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps {
            name: "ADALM-PLUTO (libiio)".to_string(),
            min_freq_hz: 70_000_000.0,
            max_freq_hz: 6_000_000_000.0,
            min_sample_rate_hz: 521_000.0,
            max_sample_rate_hz: 61_440_000.0,
            gain_range_db: (-3.0, 71.0),
            rf_ports: vec![
                "A_BALANCED".to_string(),
                "B_BALANCED".to_string(),
                "C_BALANCED".to_string(),
                "A_N".to_string(),
                "A_P".to_string(),
                "B_N".to_string(),
                "B_P".to_string(),
                "C_N".to_string(),
                "C_P".to_string(),
            ],
        }
    }

    fn configure(&mut self, config: SourceConfig) -> Result<StreamSpec> {
        if config.sample_rate_hz <= 0.0 {
            bail!("sample rate must be positive");
        }
        if config.buffer_samples == 0 {
            bail!("buffer_samples must be positive");
        }

        self.stop()?;

        let actual_freq = self.set_rx_lo(config.center_freq_hz)?;

        let rx = self.phy_channel("voltage0", false)?;

        if let Some(port) = &config.rf_port {
            Self::write_attr(rx, "rf_port_select", port)
                .with_context(|| format!("selecting RF port '{port}'"))?;
        }

        Self::write_attr(
            rx,
            "sampling_frequency",
            &format!("{:.0}", config.sample_rate_hz),
        )
        .with_context(|| format!("setting sample rate to {:.0}", config.sample_rate_hz))?;
        Self::write_attr(rx, "rf_bandwidth", &format!("{:.0}", config.bandwidth_hz))
            .with_context(|| format!("setting RF bandwidth to {:.0}", config.bandwidth_hz))?;
        Self::write_attr(rx, "gain_control_mode", config.gain_mode.as_str())
            .with_context(|| format!("setting gain mode to {}", config.gain_mode.as_str()))?;

        if config.gain_mode == GainMode::Manual {
            Self::write_attr(rx, "hardwaregain", &format!("{:.0}", config.gain_db))
                .with_context(|| format!("setting gain to {:.0} dB", config.gain_db))?;
        }

        let actual_rate =
            Self::read_f64_attr(rx, "sampling_frequency").unwrap_or(config.sample_rate_hz);
        let actual_bandwidth =
            Self::read_f64_attr(rx, "rf_bandwidth").unwrap_or(config.bandwidth_hz);
        let actual_gain = Self::read_f64_attr(rx, "hardwaregain").unwrap_or(config.gain_db);

        let i_chan = self.rx_channel("voltage0")?;
        let q_chan = self.rx_channel("voltage1")?;
        self.i_format = Some(self.data_format(i_chan)?);
        self.q_format = Some(self.data_format(q_chan)?);
        let full_scale = self.i_format.as_ref().unwrap().full_scale();

        let mut actual_config = config.clone();
        actual_config.center_freq_hz = actual_freq;
        actual_config.sample_rate_hz = actual_rate;
        actual_config.bandwidth_hz = actual_bandwidth;
        actual_config.gain_db = actual_gain;
        self.config = Some(actual_config);

        let spec = StreamSpec {
            sample_rate_hz: actual_rate,
            center_freq_hz: actual_freq,
            bandwidth_hz: actual_bandwidth,
            format: SampleFormat::Ci16Le,
            full_scale,
        };
        self.spec = Some(spec);
        Ok(spec)
    }

    fn start(&mut self) -> Result<()> {
        if !self.buffer.is_null() {
            return Ok(());
        }

        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow!("configure() must be called before start()"))?
            .clone();

        let i_chan = self.rx_channel("voltage0")?;
        let q_chan = self.rx_channel("voltage1")?;

        self.i_format = Some(self.data_format(i_chan)?);
        self.q_format = Some(self.data_format(q_chan)?);

        unsafe {
            libiio_sys::iio_channel_enable(i_chan);
            libiio_sys::iio_channel_enable(q_chan);
        }

        let buffer = unsafe {
            libiio_sys::iio_device_create_buffer(self.rx_dev, config.buffer_samples, false)
        };
        if buffer.is_null() {
            bail!("failed to create libiio RX buffer");
        }
        self.buffer = buffer;

        let step = unsafe { libiio_sys::iio_buffer_step(buffer) } as usize;
        if step < 4 {
            bail!("unexpected libiio buffer step {step} (expected at least 4 bytes)");
        }
        self.sample_step = step;

        let i_fmt = self.i_format.as_ref().unwrap();
        let q_fmt = self.q_format.as_ref().unwrap();
        if i_fmt.bits == 0 || q_fmt.bits == 0 {
            bail!("invalid channel data format");
        }

        if let Some(spec) = &mut self.spec {
            spec.full_scale = i_fmt.full_scale();
        }

        self.start_sample = 0;
        self.first_timestamp_ns = None;
        Ok(())
    }

    fn read_block(&mut self) -> Result<IqBlock> {
        if self.buffer.is_null() {
            bail!("source is not started");
        }

        let ret = unsafe { libiio_sys::iio_buffer_refill(self.buffer) };
        if ret < 0 {
            bail!("iio_buffer_refill failed (error {ret})");
        }

        let start = unsafe { libiio_sys::iio_buffer_start(self.buffer) } as *const u8;
        let end = unsafe { libiio_sys::iio_buffer_end(self.buffer) } as *const u8;
        if start.is_null() || end.is_null() || end <= start {
            bail!("libiio returned an empty buffer");
        }

        let step = self.sample_step;
        let available = (end as usize - start as usize).min(ret as usize);
        let n = available / step;
        if n == 0 {
            bail!("libiio returned no complete samples");
        }
        let overflow = (ret as usize) < (end as usize - start as usize);
        let i_fmt = self.i_format.as_ref().unwrap();
        let q_fmt = self.q_format.as_ref().unwrap();
        let i_scale = i_fmt.full_scale() as f32;
        let q_scale = q_fmt.full_scale() as f32;

        let mut samples = Vec::with_capacity(n);
        for idx in 0..n {
            let ptr = unsafe { start.add(idx * step) };
            let i_raw = unsafe { ptr::read_unaligned(ptr as *const i16) };
            let q_raw = unsafe { ptr::read_unaligned(ptr.add(2) as *const i16) };

            let i_val = (i_raw >> i_fmt.shift) as f32 / i_scale;
            let q_val = (q_raw >> q_fmt.shift) as f32 / q_scale;
            samples.push(Complex32::new(i_val, q_val));
        }

        let spec = self
            .spec
            .ok_or_else(|| anyhow!("source has no stream spec"))?;
        let first_timestamp_ns = *self.first_timestamp_ns.get_or_insert_with(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        });
        let offset_ns = (self.start_sample as f64 * 1_000_000_000.0 / spec.sample_rate_hz) as u64;
        let timestamp_ns = Some(first_timestamp_ns.saturating_add(offset_ns));

        let block = IqBlock {
            samples: Arc::from(samples),
            spec,
            start_sample: self.start_sample,
            timestamp_ns,
            flags: midori_core::BlockFlags {
                overflow,
                discontinuity: false,
            },
        };
        self.start_sample += block.len() as u64;
        Ok(block)
    }

    fn stop(&mut self) -> Result<()> {
        if !self.buffer.is_null() {
            unsafe {
                libiio_sys::iio_buffer_destroy(self.buffer);
            }
            self.buffer = ptr::null_mut();
        }
        Ok(())
    }

    fn set_frequency(&mut self, hz: f64) -> Result<()> {
        let actual = self.set_rx_lo(hz)?;
        if let Some(config) = &mut self.config {
            config.center_freq_hz = actual;
        }
        if let Some(spec) = &mut self.spec {
            spec.center_freq_hz = actual;
        }
        Ok(())
    }

    fn set_gain(&mut self, db: f64) -> Result<()> {
        let rx = self.phy_channel("voltage0", false)?;
        Self::write_attr(rx, "hardwaregain", &format!("{:.0}", db))?;
        if let Some(config) = &mut self.config {
            config.gain_db = db;
        }
        Ok(())
    }
}
