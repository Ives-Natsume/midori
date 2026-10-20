//! SSDV decoding (DSLWP and classic packets).
//!
//! [`Bpsk9600Receiver`] is an optional 9k6 BPSK / CCSDS concatenated front-end
//! (validated with ASRTU-1) built on OpenHoshimi's soundmodem. The SSDV layer
//! itself is link-agnostic and accepts:
//!
//! * 218-byte DSLWP packets (9-byte header, 205-byte JPEG payload, CRC32),
//! * classic 256-byte SSDV packets (FEC and no-FEC) for other spacecraft,
//! * caller-supplied CRC initial values; the polynomial/xor-out are fixed by
//!   SSDV, but implementations disagree on the initial value,
//! * per-image packet collection and gap filling,
//! * SSDV scan data to JPEG, then JPEG to PNG for the Midori UI.
//!
//! The JPEG assembler is OpenHoshimi's `ssdv_jpeg` module. OpenHoshimi is
//! GPL-3.0-or-later, which is why this sidecar carries its own license rather
//! than the workspace's MIT/Apache-2.0 default. Satellite-specific values
//! (CRC initial value, frame header length) are supplied by the caller.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use jpeg_decoder::PixelFormat;
use midori_core::Sample;
use midori_plugin::StatusLevel;
use openhoshimi_codec::ssdv::{SsdvDecoder, SsdvPacket, SsdvPacketKind};
use openhoshimi_codec::ssdv_jpeg::JpegBuilder;

/// Length of one RS(255,223) decoded CCSDS frame.
pub const CCSDS_FRAME_LEN: usize = 223;
/// Length of one 218-byte DSLWP-variant SSDV packet.
pub const DSLWP_PACKET_LEN: usize = 218;
/// DSLWP SSDV header length.
pub const DSLWP_HEADER_LEN: usize = 9;
/// DSLWP SSDV JPEG payload length.
pub const DSLWP_PAYLOAD_LEN: usize = 205;
/// Classic SSDV packet length.
pub const STANDARD_PACKET_LEN: usize = 256;

/// CRC32 initial value used by classic SSDV.
pub const DEFAULT_CRC_INIT: u32 = 0xFFFF_FFFF;

const PREVIEW_INTERVAL: Duration = Duration::from_millis(650);
const MAX_JPEG_BYTES: usize = 32 * 1024 * 1024;

/// A validated SSDV packet in a common form for image assembly.
#[derive(Debug, Clone)]
pub struct PacketInfo {
    pub image_id: u8,
    pub packet_id: u16,
    pub width: u16,
    pub height: u16,
    pub eoi: bool,
    pub quality: u8,
    pub mcu_mode: u8,
    pub mcu_offset: u8,
    pub mcu_id: u16,
    pub mcu_count: u32,
    pub payload: Vec<u8>,
    pub raw: Vec<u8>,
    /// CRC32 initial value that validated this packet (`None` for the
    /// classic 256-byte decoder, which owns its own variant handling).
    pub crc_init: Option<u32>,
}

impl PacketInfo {
    fn to_hoshimi(&self) -> SsdvPacket {
        // `JpegBuilder` reads the width/height codes from raw[9]/raw[10]
        // instead of using `Self::mcu_count`. A DSLWP packet has a different
        // header layout, so provide a standard-layout shim there; otherwise
        // the builder pads the JPEG to a bogus (much larger) MCU count.
        let mut raw = self.raw.clone();
        if raw.len() >= 11 {
            raw[9] = (self.width >> 4) as u8;
            raw[10] = (self.height >> 4) as u8;
        }
        SsdvPacket {
            kind: SsdvPacketKind::NoFec,
            callsign: String::new(),
            image_id: self.image_id,
            packet_id: self.packet_id,
            width: self.width,
            height: self.height,
            eoi: self.eoi,
            quality: self.quality,
            mcu_mode: self.mcu_mode,
            mcu_offset: self.mcu_offset,
            mcu_id: self.mcu_id,
            mcu_count: self.mcu_count,
            payload: self.payload.clone(),
            rs_errors: None,
            crc_ok: true,
            raw,
        }
    }
}

/// A completed or in-progress image update.
#[derive(Debug, Clone)]
pub struct SsdvImage {
    pub image_id: u8,
    pub width: u16,
    pub height: u16,
    pub quality: u8,
    pub received_packets: usize,
    pub missing_packets: usize,
    pub crc_errors: u64,
    pub complete: bool,
    pub jpeg: Vec<u8>,
    pub png: Vec<u8>,
    pub jpeg_path: Option<PathBuf>,
    pub png_path: Option<PathBuf>,
    pub packet_path: Option<PathBuf>,
}

/// Event emitted by an SSDV session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Status {
        level: StatusLevel,
        message: String,
    },
    Progress {
        title: String,
        image_id: u8,
        received: usize,
        total_hint: usize,
        missing: usize,
        fraction: f32,
    },
    Image(SsdvImage),
}

/// Parse one 218-byte DSLWP SSDV packet.
///
/// `crc_inits` is the list of CRC32 initial values accepted for this stream.
/// The polynomial (`0xEDB88320`) and final XOR are fixed by SSDV; only the
/// initial value differs between implementations.
pub fn parse_dslwp_packet(raw: &[u8], crc_inits: &[u32]) -> Result<PacketInfo> {
    if raw.len() != DSLWP_PACKET_LEN {
        bail!(
            "DSLWP packet must be {DSLWP_PACKET_LEN} bytes, got {}",
            raw.len()
        );
    }
    let stored_crc = u32::from_be_bytes(raw[214..218].try_into().unwrap());
    let crc_init = crc_inits
        .iter()
        .copied()
        .find(|initial| crc32(&raw[..DSLWP_PACKET_LEN - 4], *initial) == stored_crc)
        .ok_or_else(|| {
            anyhow!(
                "DSLWP SSDV CRC32 mismatch (tried {} initial value(s))",
                crc_inits.len()
            )
        })?;

    let width_code = raw[3];
    let height_code = raw[4];
    if width_code == 0 || height_code == 0 {
        bail!("DSLWP SSDV header has zero dimensions");
    }
    let width = u16::from(width_code) << 4;
    let height = u16::from(height_code) << 4;
    let flags = raw[5];
    let mcu_mode = flags & 0x03;
    let mcu_count = mcu_count(width_code, height_code, mcu_mode);
    if mcu_count == 0 || mcu_count > u32::from(u16::MAX) {
        bail!("DSLWP SSDV MCU count {mcu_count} outside the protocol limit");
    }
    let mcu_id = u16::from_be_bytes([raw[7], raw[8]]);
    let mcu_offset = raw[6];
    if mcu_id != 0xFFFF {
        if u32::from(mcu_id) >= mcu_count {
            bail!("DSLWP SSDV MCU id {mcu_id} outside {mcu_count}");
        }
        if usize::from(mcu_offset) >= DSLWP_PAYLOAD_LEN {
            bail!("DSLWP SSDV MCU offset {mcu_offset} outside payload");
        }
    }

    Ok(PacketInfo {
        image_id: raw[0],
        packet_id: u16::from_be_bytes([raw[1], raw[2]]),
        width,
        height,
        eoi: flags & 0x04 != 0,
        quality: ((flags >> 3) & 0x07) ^ 0x04,
        mcu_mode,
        mcu_offset,
        mcu_id,
        mcu_count,
        payload: raw[DSLWP_HEADER_LEN..DSLWP_PACKET_LEN - 4].to_vec(),
        raw: raw.to_vec(),
        crc_init: Some(crc_init),
    })
}

/// Validate and parse one classic 256-byte SSDV packet, including the
/// Reed-Solomon FEC variant.
pub fn parse_standard_packet(raw: &[u8]) -> Result<PacketInfo> {
    if raw.len() != STANDARD_PACKET_LEN {
        bail!(
            "standard SSDV packet must be {STANDARD_PACKET_LEN} bytes, got {}",
            raw.len()
        );
    }
    let packet = SsdvDecoder::new()
        .decode(raw)
        .map_err(|error| anyhow!("SSDV packet rejected: {error}"))?;
    if packet.mcu_count == 0 || packet.mcu_count > u32::from(u16::MAX) {
        bail!(
            "SSDV MCU count {} outside the image-assembly limit",
            packet.mcu_count
        );
    }
    Ok(PacketInfo {
        image_id: packet.image_id,
        packet_id: packet.packet_id,
        width: packet.width,
        height: packet.height,
        eoi: packet.eoi,
        quality: packet.quality,
        mcu_mode: packet.mcu_mode,
        mcu_offset: packet.mcu_offset,
        mcu_id: packet.mcu_id,
        mcu_count: packet.mcu_count,
        payload: packet.payload,
        raw: packet.raw,
        crc_init: None,
    })
}

/// Parse a single CRC initial value. Accepts decimal or `0x`-prefixed hex.
pub fn parse_crc_init(text: &str) -> Option<u32> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

/// Parse a user-supplied CRC initial list. Each entry may contain one value or
/// a comma-separated group. Empty lists fall back to classic SSDV.
pub fn parse_crc_inits(values: &[String]) -> Vec<u32> {
    let mut inits = Vec::new();
    for value in values {
        for part in value.split(',') {
            if let Some(init) = parse_crc_init(part)
                && !inits.contains(&init)
            {
                inits.push(init);
            }
        }
    }
    if inits.is_empty() {
        inits.push(DEFAULT_CRC_INIT);
    }
    inits
}

/// Reflected CRC32 used by SSDV (poly 0xEDB88320, xor-out 0xFFFFFFFF).
pub fn crc32(data: &[u8], initial: u32) -> u32 {
    let mut crc = initial;
    for &byte in data {
        let mut value = (crc ^ u32::from(byte)) & 0xFF;
        for _ in 0..8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xEDB8_8320
            } else {
                value >> 1
            };
        }
        crc = (crc >> 8) ^ value;
    }
    crc ^ 0xFFFF_FFFF
}

fn mcu_count(width_code: u8, height_code: u8, mcu_mode: u8) -> u32 {
    let mut count = u32::from(width_code) * u32::from(height_code);
    match mcu_mode {
        1 | 2 => count *= 2,
        3 => count *= 4,
        _ => {}
    }
    count
}

struct ImageState {
    image_id: u8,
    width: u16,
    height: u16,
    quality: u8,
    mcu_mode: u8,
    mcu_count: u32,
    packets: BTreeMap<u16, PacketInfo>,
    complete: bool,
    progress: f32,
    first_seen: Instant,
    last_preview: Option<Instant>,
    jpeg_path: PathBuf,
    png_path: PathBuf,
    packet_path: PathBuf,
    save: bool,
}

impl ImageState {
    fn new(image_id: u8, packet: &PacketInfo, serial: u64, output_dir: Option<&Path>) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let base = format!("ssdv_{stamp}_{serial:03}_ID{image_id}");
        let dir = output_dir.unwrap_or_else(|| Path::new("."));
        Self {
            image_id,
            width: packet.width,
            height: packet.height,
            quality: packet.quality,
            mcu_mode: packet.mcu_mode,
            mcu_count: packet.mcu_count,
            packets: BTreeMap::new(),
            complete: false,
            progress: 0.0,
            first_seen: Instant::now(),
            last_preview: None,
            jpeg_path: dir.join(format!("{base}.jpg")),
            png_path: dir.join(format!("{base}.png")),
            packet_path: dir.join(format!("{base}.bin")),
            save: output_dir.is_some(),
        }
    }

    fn matches(&self, packet: &PacketInfo) -> bool {
        self.width == packet.width
            && self.height == packet.height
            && self.quality == packet.quality
            && self.mcu_mode == packet.mcu_mode
    }

    /// Estimate the total packet count from the packet-id to MCU-id slope.
    ///
    /// SSDV does not store the image's packet count; only the total MCU count
    /// and the first MCU of each packet. Packet ids and MCU ids advance
    /// linearly, so two or more packets are enough to extrapolate the packet
    /// id at the end of the image. The estimate starts progress near zero even
    /// when reception begins in the middle of an image, instead of treating
    /// the first packet id as a completion target.
    fn estimated_total_packets(&self) -> Option<usize> {
        let mut points: Vec<(f64, f64)> = self
            .packets
            .values()
            .filter(|packet| packet.mcu_id != 0xFFFF)
            .map(|packet| (f64::from(packet.packet_id), f64::from(packet.mcu_id)))
            .collect();
        points.sort_by(|left, right| left.0.total_cmp(&right.0));

        if points.len() >= 2 {
            let (first_id, first_mcu) = points[0];
            let (last_id, last_mcu) = points[points.len() - 1];
            let id_span = last_id - first_id;
            let mcu_span = last_mcu - first_mcu;
            if id_span > 0.0 && mcu_span > 0.0 {
                let total =
                    first_id + (f64::from(self.mcu_count) - first_mcu) / (mcu_span / id_span);
                if total.is_finite() {
                    return Some(
                        (total
                            .ceil()
                            .max(last_id + 1.0)
                            .max(self.packets.len() as f64)) as usize,
                    );
                }
            }
        }

        None
    }

    /// Monotonic image-completeness estimate in `0.0..=1.0` and the total
    /// packet estimate used to derive it. A single packet does not carry
    /// enough information to extrapolate the total, so callers should wait
    /// for a second packet before showing a progress bar.
    fn update_progress(&mut self) -> Option<(f32, usize)> {
        let total = self
            .estimated_total_packets()?
            .max(self.packets.len())
            .max(1);
        if self.complete && self.missing() == 0 {
            self.progress = 1.0;
            return Some((self.progress, total));
        }
        let fraction = (self.packets.len() as f32 / total as f32).clamp(0.0, 1.0);
        self.progress = self.progress.max(fraction);
        Some((self.progress, total))
    }

    fn missing(&self) -> usize {
        let last = self.packets.keys().next_back().copied().unwrap_or(0);
        (0..=last)
            .filter(|id| !self.packets.contains_key(id))
            .count()
    }

    fn build_jpeg(&self) -> Option<Vec<u8>> {
        if self.packets.is_empty() {
            return None;
        }
        let mut builder = JpegBuilder::new();
        for packet in self.packets.values() {
            builder.feed_packet(&packet.to_hoshimi());
        }
        let jpeg = builder.finish();
        (!jpeg.is_empty() && jpeg.len() <= MAX_JPEG_BYTES).then_some(jpeg)
    }

    fn save_packet_bin(&self) -> Result<()> {
        if !self.save {
            return Ok(());
        }
        let mut bytes = Vec::new();
        for packet in self.packets.values() {
            bytes.extend_from_slice(&packet.raw);
        }
        if let Some(parent) = self.packet_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.packet_path, bytes)
            .with_context(|| format!("writing {}", self.packet_path.display()))
    }
}

/// Stateful SSDV receiver. Feed it 223-byte CCSDS frames, 218-byte DSLWP
/// packets, or 256-byte classic packets.
pub struct SsdvSession {
    output_dir: Option<PathBuf>,
    images: BTreeMap<u8, ImageState>,
    serial: u64,
    preview_interval: Duration,
    crc_inits: Vec<u32>,
    frame_header_len: usize,
    crc_errors: u64,
    ignored_frames: u64,
}

impl SsdvSession {
    /// Classic SSDV defaults: CRC init `0xFFFFFFFF`, five-byte CCSDS header.
    pub fn new(output_dir: Option<PathBuf>) -> Self {
        Self::with_config(output_dir, vec![DEFAULT_CRC_INIT], 5)
    }

    /// Use a caller-supplied set of CRC initial values with the default
    /// five-byte CCSDS header.
    pub fn with_crc_inits(output_dir: Option<PathBuf>, crc_inits: Vec<u32>) -> Self {
        Self::with_config(output_dir, crc_inits, 5)
    }

    /// Full configuration for a stream whose CRC initial value or CCSDS
    /// header length differs from the classic defaults.
    pub fn with_config(
        output_dir: Option<PathBuf>,
        crc_inits: Vec<u32>,
        frame_header_len: usize,
    ) -> Self {
        let mut unique = Vec::with_capacity(crc_inits.len());
        for init in crc_inits {
            if !unique.contains(&init) {
                unique.push(init);
            }
        }
        let mut crc_inits = unique;
        if crc_inits.is_empty() {
            crc_inits.push(DEFAULT_CRC_INIT);
        }
        Self {
            output_dir,
            images: BTreeMap::new(),
            serial: 0,
            preview_interval: PREVIEW_INTERVAL,
            crc_inits,
            frame_header_len: frame_header_len.min(CCSDS_FRAME_LEN - DSLWP_PACKET_LEN),
            crc_errors: 0,
            ignored_frames: 0,
        }
    }

    pub fn set_output_dir(&mut self, output_dir: Option<PathBuf>) {
        self.output_dir = output_dir;
    }

    pub fn set_preview_interval(&mut self, interval: Duration) {
        self.preview_interval = interval;
    }

    /// Replace the accepted CRC initial values. An empty list restores the
    /// classic SSDV default.
    pub fn set_crc_inits(&mut self, crc_inits: Vec<u32>) {
        self.crc_inits = if crc_inits.is_empty() {
            vec![DEFAULT_CRC_INIT]
        } else {
            crc_inits
        };
    }

    pub fn crc_inits(&self) -> &[u32] {
        &self.crc_inits
    }

    /// Number of bytes before the 218-byte SSDV packet inside a 223-byte
    /// frame. Defaults to 5 (CCSDS frame header).
    pub fn set_frame_header_len(&mut self, len: usize) {
        self.frame_header_len = len.min(CCSDS_FRAME_LEN - DSLWP_PACKET_LEN);
    }

    pub fn frame_header_len(&self) -> usize {
        self.frame_header_len
    }

    pub fn crc_errors(&self) -> u64 {
        self.crc_errors
    }

    pub fn ignored_frames(&self) -> u64 {
        self.ignored_frames
    }

    /// Process one 223-byte CCSDS frame and extract the SSDV packet after the
    /// configured header. No virtual-channel assumption is made; the packet
    /// CRC decides whether the frame belongs to an SSDV stream.
    pub fn process_frame(&mut self, frame: &[u8]) -> Vec<SessionEvent> {
        if frame.len() != CCSDS_FRAME_LEN {
            self.ignored_frames += 1;
            return Vec::new();
        }
        let start = self.frame_header_len;
        let Some(packet) = frame.get(start..start + DSLWP_PACKET_LEN) else {
            self.ignored_frames += 1;
            return Vec::new();
        };
        self.process_dslwp_packet(packet)
    }

    pub fn process_dslwp_packet(&mut self, packet: &[u8]) -> Vec<SessionEvent> {
        match parse_dslwp_packet(packet, &self.crc_inits) {
            Ok(packet) => self.ingest(packet),
            Err(error) => {
                self.crc_errors += 1;
                if self.crc_errors == 1 || self.crc_errors.is_multiple_of(25) {
                    vec![SessionEvent::Status {
                        level: StatusLevel::Warning,
                        message: format!(
                            "SSDV packet rejected ({error}); total {}",
                            self.crc_errors
                        ),
                    }]
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn process_standard_packet(&mut self, packet: &[u8]) -> Vec<SessionEvent> {
        match parse_standard_packet(packet) {
            Ok(packet) => self.ingest(packet),
            Err(error) => {
                self.crc_errors += 1;
                if self.crc_errors == 1 || self.crc_errors.is_multiple_of(25) {
                    vec![SessionEvent::Status {
                        level: StatusLevel::Warning,
                        message: format!(
                            "SSDV packet rejected ({error}); total {}",
                            self.crc_errors
                        ),
                    }]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Auto-detect the layout of a byte block. Useful for replaying raw frame
    /// dumps without metadata.
    pub fn process_bytes(&mut self, bytes: &[u8]) -> Result<Vec<SessionEvent>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let (item_len, frame_mode) = if bytes.len().is_multiple_of(CCSDS_FRAME_LEN) {
            (CCSDS_FRAME_LEN, true)
        } else if bytes.len().is_multiple_of(DSLWP_PACKET_LEN) {
            (DSLWP_PACKET_LEN, false)
        } else if bytes.len().is_multiple_of(STANDARD_PACKET_LEN) {
            (STANDARD_PACKET_LEN, false)
        } else {
            bail!(
                "byte block {} is not a multiple of 223, 218, or 256 bytes",
                bytes.len()
            );
        };

        let mut events = Vec::new();
        for chunk in bytes.chunks_exact(item_len) {
            let mut batch = if frame_mode {
                self.process_frame(chunk)
            } else if item_len == DSLWP_PACKET_LEN {
                self.process_dslwp_packet(chunk)
            } else {
                self.process_standard_packet(chunk)
            };
            events.append(&mut batch);
        }
        Ok(events)
    }

    fn ingest(&mut self, packet: PacketInfo) -> Vec<SessionEvent> {
        let image_id = packet.image_id;
        let mut reset = false;

        if let Some(state) = self.images.get(&image_id) {
            if !state.matches(&packet) {
                reset = true;
            } else if packet.packet_id == 0 {
                // A fresh, different packet 0 means the sender restarted the
                // image. A duplicate is ignored.
                if let Some(existing) = state.packets.get(&0) {
                    if existing.raw != packet.raw {
                        reset = true;
                    } else {
                        return Vec::new();
                    }
                }
            }
        }

        if reset || !self.images.contains_key(&image_id) {
            self.serial += 1;
            let state = ImageState::new(image_id, &packet, self.serial, self.output_dir.as_deref());
            self.images.insert(image_id, state);
        }

        let Some(state) = self.images.get_mut(&image_id) else {
            return Vec::new();
        };
        let previous = state.packets.get(&packet.packet_id);
        if previous.is_some_and(|existing| existing.raw == packet.raw) {
            return Vec::new();
        }
        state.packets.insert(packet.packet_id, packet.clone());
        state.complete |= packet.eoi;

        if let Err(error) = state.save_packet_bin() {
            return vec![SessionEvent::Status {
                level: StatusLevel::Warning,
                message: format!("SSDV packet log: {error}"),
            }];
        }

        let now = Instant::now();
        let due = state.complete
            || state
                .last_preview
                .is_none_or(|last| now.duration_since(last) >= self.preview_interval);
        if !due {
            return Vec::new();
        }
        state.last_preview = Some(now);

        let missing = state.missing();
        let mut events = Vec::new();
        if !state.complete {
            let (fraction, total_hint) = state.update_progress().unwrap_or((0.0, 0));
            events.push(SessionEvent::Progress {
                title: format!(
                    "SSDV ID {} {}x{}",
                    state.image_id, state.width, state.height
                ),
                image_id: state.image_id,
                received: state.packets.len(),
                total_hint,
                missing,
                fraction,
            });
        }

        let Some(jpeg) = state.build_jpeg() else {
            return events;
        };
        let (width, height, rgb) = match decode_jpeg(&jpeg) {
            Ok(decoded) => decoded,
            Err(error) => {
                events.push(SessionEvent::Status {
                    level: StatusLevel::Warning,
                    message: format!("SSDV JPEG decode: {error}"),
                });
                return events;
            }
        };
        let png = match encode_png(width, height, &rgb) {
            Ok(png) => png,
            Err(error) => {
                events.push(SessionEvent::Status {
                    level: StatusLevel::Warning,
                    message: format!("SSDV PNG encode: {error}"),
                });
                return events;
            }
        };

        if state.save {
            if let Some(parent) = state.jpeg_path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                events.push(SessionEvent::Status {
                    level: StatusLevel::Warning,
                    message: format!("SSDV output directory: {error}"),
                });
            }
            if let Err(error) = std::fs::write(&state.jpeg_path, &jpeg) {
                events.push(SessionEvent::Status {
                    level: StatusLevel::Warning,
                    message: format!("SSDV JPEG: {error}"),
                });
            }
            if state.complete {
                let _ = std::fs::write(&state.png_path, &png);
            }
        }

        let width = u16::try_from(width).unwrap_or(u16::MAX);
        let height = u16::try_from(height).unwrap_or(u16::MAX);
        events.push(SessionEvent::Image(SsdvImage {
            image_id: state.image_id,
            width,
            height,
            quality: state.quality,
            received_packets: state.packets.len(),
            missing_packets: state.missing(),
            crc_errors: self.crc_errors,
            complete: state.complete,
            jpeg,
            png,
            jpeg_path: state.save.then(|| state.jpeg_path.clone()),
            png_path: (state.save && state.complete).then(|| state.png_path.clone()),
            packet_path: state.save.then(|| state.packet_path.clone()),
        }));
        events
    }

    /// Seconds since the oldest image started, for diagnostics.
    pub fn oldest_image_age(&self) -> Option<Duration> {
        self.images
            .values()
            .map(|state| state.first_seen.elapsed())
            .min()
    }

    /// Reset packet accumulation while keeping the output directory.
    pub fn clear(&mut self) {
        self.images.clear();
    }
}

/// 9k6 BPSK / CCSDS concatenated receiver: IQ in, SSDV events out.
///
/// This is a convenience front-end for a 9k6 BPSK/CCSDS link (validated with
/// ASRTU-1). The SSDV session it owns is independent of any satellite.
pub struct Bpsk9600Receiver {
    modem: openhoshimi_soundmodem::Decoder,
    session: SsdvSession,
    interleaved: Vec<f32>,
    frames: u64,
    rs_corrected: i64,
}

impl Bpsk9600Receiver {
    pub fn new(output_dir: Option<PathBuf>) -> Result<Self> {
        Self::with_config(output_dir, vec![DEFAULT_CRC_INIT], 5)
    }

    pub fn with_crc_inits(output_dir: Option<PathBuf>, crc_inits: Vec<u32>) -> Result<Self> {
        Self::with_config(output_dir, crc_inits, 5)
    }

    pub fn with_config(
        output_dir: Option<PathBuf>,
        crc_inits: Vec<u32>,
        frame_header_len: usize,
    ) -> Result<Self> {
        let modem = openhoshimi_soundmodem::Decoder::new()
            .ok_or_else(|| anyhow!("could not allocate the 9k6 BPSK soundmodem decoder"))?;
        Ok(Self {
            modem,
            session: SsdvSession::with_config(output_dir, crc_inits, frame_header_len),
            interleaved: Vec::new(),
            frames: 0,
            rs_corrected: 0,
        })
    }

    pub fn session(&self) -> &SsdvSession {
        &self.session
    }

    pub fn set_crc_inits(&mut self, crc_inits: Vec<u32>) {
        self.session.set_crc_inits(crc_inits);
    }

    pub fn set_frame_header_len(&mut self, len: usize) {
        self.session.set_frame_header_len(len);
    }

    pub fn frame_count(&self) -> u64 {
        self.frames
    }

    pub fn rs_corrected_bytes(&self) -> i64 {
        self.rs_corrected
    }

    /// Feed complex baseband at 48 kHz, centred on the signal.
    pub fn process_iq(&mut self, samples: &[Sample]) -> Vec<SessionEvent> {
        self.interleaved.clear();
        self.interleaved.reserve(samples.len() * 2);
        for sample in samples {
            self.interleaved.push(sample.re);
            self.interleaved.push(sample.im);
        }
        self.modem.feed_iq(&self.interleaved);
        self.drain()
    }

    /// Feed the decoder's trailing partial block at end of stream.
    pub fn flush(&mut self) -> Vec<SessionEvent> {
        self.modem.flush();
        self.drain()
    }

    fn drain(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        while let Some(frame) = self.modem.poll() {
            self.frames += 1;
            self.rs_corrected += i64::from(frame.corrected.max(0));
            let mut batch = self.session.process_frame(&frame.data);
            events.append(&mut batch);
        }
        events
    }
}

/// 9k6 BPSK / CCSDS receiver for 48 kHz mono 12 kHz-IF audio.
///
/// Some receivers expose a real 12 kHz intermediate-frequency recording
/// instead of zero-IF I/Q. This wrapper covers that input; the offline tool
/// uses it, while the live sidecar stays on the lossless IQ path.
pub struct Bpsk9600AudioReceiver {
    modem: openhoshimi_soundmodem::Decoder,
    session: SsdvSession,
    pcm: Vec<i16>,
    frames: u64,
    rs_corrected: i64,
}

impl Bpsk9600AudioReceiver {
    pub fn new(output_dir: Option<PathBuf>) -> Result<Self> {
        Self::with_config(output_dir, vec![DEFAULT_CRC_INIT], 5)
    }

    pub fn with_crc_inits(output_dir: Option<PathBuf>, crc_inits: Vec<u32>) -> Result<Self> {
        Self::with_config(output_dir, crc_inits, 5)
    }

    pub fn with_config(
        output_dir: Option<PathBuf>,
        crc_inits: Vec<u32>,
        frame_header_len: usize,
    ) -> Result<Self> {
        let modem = openhoshimi_soundmodem::Decoder::new()
            .ok_or_else(|| anyhow!("could not allocate the 9k6 BPSK soundmodem decoder"))?;
        Ok(Self {
            modem,
            session: SsdvSession::with_config(output_dir, crc_inits, frame_header_len),
            pcm: Vec::new(),
            frames: 0,
            rs_corrected: 0,
        })
    }

    pub fn set_crc_inits(&mut self, crc_inits: Vec<u32>) {
        self.session.set_crc_inits(crc_inits);
    }

    pub fn set_frame_header_len(&mut self, len: usize) {
        self.session.set_frame_header_len(len);
    }

    pub fn frame_count(&self) -> u64 {
        self.frames
    }

    pub fn rs_corrected_bytes(&self) -> i64 {
        self.rs_corrected
    }

    /// Feed 48 kHz real samples with the signal centred on 12 kHz.
    pub fn process_mono(&mut self, samples: &[f32]) -> Vec<SessionEvent> {
        self.pcm.clear();
        self.pcm.extend(
            samples
                .iter()
                .map(|sample| (sample.clamp(-1.0, 1.0) * 32767.0) as i16),
        );
        self.modem.feed(&self.pcm);
        self.drain()
    }

    /// Feed complex samples from a mono source (Q is ignored).
    pub fn process_audio(&mut self, samples: &[Sample]) -> Vec<SessionEvent> {
        self.pcm.clear();
        self.pcm.extend(
            samples
                .iter()
                .map(|sample| (sample.re.clamp(-1.0, 1.0) * 32767.0) as i16),
        );
        self.modem.feed(&self.pcm);
        self.drain()
    }

    pub fn flush(&mut self) -> Vec<SessionEvent> {
        self.modem.flush();
        self.drain()
    }

    fn drain(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        while let Some(frame) = self.modem.poll() {
            self.frames += 1;
            self.rs_corrected += i64::from(frame.corrected.max(0));
            let mut batch = self.session.process_frame(&frame.data);
            events.append(&mut batch);
        }
        events
    }
}

/// Decode a JPEG produced by the SSDV assembler to packed RGB8.
pub fn decode_jpeg(jpeg: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(jpeg));
    let pixels = decoder.decode().context("decoding SSDV JPEG")?;
    let info = decoder
        .info()
        .ok_or_else(|| anyhow!("SSDV JPEG has no frame info"))?;
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    let rgb = match info.pixel_format {
        PixelFormat::RGB24 => pixels,
        PixelFormat::L8 => pixels
            .into_iter()
            .flat_map(|value| [value, value, value])
            .collect(),
        other => bail!("unsupported SSDV JPEG pixel format {other:?}"),
    };
    let expected = width as usize * height as usize * 3;
    if rgb.len() != expected {
        bail!("SSDV JPEG has {} RGB bytes, expected {expected}", rgb.len());
    }
    Ok((width, height, rgb))
}

/// Encode packed RGB8 as a PNG for the generic sidecar image event.
pub fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("writing PNG header")?;
        writer.write_image_data(rgb).context("writing PNG pixels")?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CRC_INIT: u32 = 0x4EE4_FDE1;
    const ALT_CRC_INIT: u32 = 0x6AAA_C1C5;

    fn make_dslwp_packet(image_id: u8, packet_id: u16) -> Vec<u8> {
        let mut packet = vec![0u8; DSLWP_PACKET_LEN];
        packet[0] = image_id;
        packet[1..3].copy_from_slice(&packet_id.to_be_bytes());
        packet[3] = 20; // 320 px
        packet[4] = 15; // 240 px
        packet[5] = 0x38; // quality 3, 2x2 MCU
        packet[6] = 0;
        packet[7..9].copy_from_slice(&0u16.to_be_bytes());
        let crc = crc32(&packet[..DSLWP_PACKET_LEN - 4], TEST_CRC_INIT);
        packet[214..218].copy_from_slice(&crc.to_be_bytes());
        packet
    }

    #[test]
    fn parses_dslwp_header_and_crc() {
        let packet = make_dslwp_packet(43, 34);
        let parsed = parse_dslwp_packet(&packet, &[TEST_CRC_INIT]).unwrap();
        assert_eq!(parsed.image_id, 43);
        assert_eq!(parsed.packet_id, 34);
        assert_eq!((parsed.width, parsed.height), (320, 240));
        assert_eq!(parsed.payload.len(), DSLWP_PAYLOAD_LEN);
        assert_eq!(parsed.crc_init, Some(TEST_CRC_INIT));
        assert_eq!(parsed.mcu_count, 300);
    }

    #[test]
    fn rejects_corrupt_dslwp_crc() {
        let mut packet = make_dslwp_packet(1, 0);
        packet[20] ^= 0xFF;
        assert!(parse_dslwp_packet(&packet, &[TEST_CRC_INIT]).is_err());
    }

    #[test]
    fn alternate_crc_init_is_accepted() {
        let mut packet = make_dslwp_packet(1, 0);
        let crc = crc32(&packet[..DSLWP_PACKET_LEN - 4], ALT_CRC_INIT);
        packet[214..218].copy_from_slice(&crc.to_be_bytes());
        let parsed = parse_dslwp_packet(&packet, &[TEST_CRC_INIT, ALT_CRC_INIT]).unwrap();
        assert_eq!(parsed.crc_init, Some(ALT_CRC_INIT));
    }

    #[test]
    fn crc_init_list_accepts_hex_and_decimal() {
        let parsed = parse_crc_inits(&[
            "0x4ee4fde1, 0x6AAAC1C5".to_string(),
            "4294967295".to_string(),
            "bogus".to_string(),
        ]);
        assert_eq!(parsed, vec![TEST_CRC_INIT, ALT_CRC_INIT, DEFAULT_CRC_INIT]);
        assert_eq!(parse_crc_inits(&[]), vec![DEFAULT_CRC_INIT]);
    }

    #[test]
    fn frame_header_length_is_configurable() {
        let mut session = SsdvSession::with_crc_inits(None, vec![TEST_CRC_INIT]);
        session.set_frame_header_len(0);
        let mut frame = make_dslwp_packet(1, 0);
        frame.extend_from_slice(&[0u8; 5]);
        // The packet is at the start, so a zero header length is valid for a
        // frame whose first 218 bytes are the packet.
        let events = session.process_frame(&frame[..CCSDS_FRAME_LEN]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Image(_)))
        );
    }

    #[test]
    fn byte_autodetect_requires_a_whole_block() {
        let mut session = SsdvSession::new(None);
        assert!(session.process_bytes(&[0u8; 100]).is_err());
    }
}
