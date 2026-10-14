//! LATM/LOAS -> ADTS rewriting for the audio stream the REMUX emits.
//!
//! The REMUX packs the BS4K audio as MPEG-4 AAC in LOAS/LATM (PMT stream_type
//! 0x11). ffmpeg, VLC and mpv decode that, but TVTest does not: every other
//! Japanese broadcast carries AAC in ADTS (stream_type 0x0F), and TVTest only
//! handles that. Measured with the same capture: LATM plays video with no
//! sound in TVTest, the ADTS remux of it plays with sound. dantto4k converts
//! to ADTS by default for the same reason.
//!
//! The conversion does not re-encode anything. Each LATM AudioMuxElement
//! carries one or more raw AAC frames plus (usually) a StreamMuxConfig with
//! the AudioSpecificConfig; every raw frame gets a 7-byte ADTS header built
//! from that config. PES headers (and so PTS/DTS) are kept as they are, only
//! the PES payload and PES_packet_length change, and the audio PID is
//! re-packetized with its own continuity counter. PMT stream_type 0x11 is
//! rewritten to 0x0F with a fresh CRC.

use crate::remux4k::{AUDIO_PID, PMT_PID};

const TS: usize = 188;
pub const STREAM_TYPE_LATM: u8 = 0x11;
pub const STREAM_TYPE_ADTS: u8 = 0x0F;

// --- bit reader -------------------------------------------------------------

struct Bits<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    fn left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    fn read(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 || self.left() < n as usize {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.data[self.pos / 8];
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        if self.left() < n {
            return None;
        }
        self.pos += n;
        Some(())
    }

    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// `LatmGetValue()` from ISO/IEC 14496-3 1.7.3.
    fn latm_value(&mut self) -> Option<u32> {
        let bytes = self.read(2)?;
        let mut v = 0u32;
        for _ in 0..=bytes {
            v = (v << 8) | self.read(8)?;
        }
        Some(v)
    }
}

// --- AudioSpecificConfig ----------------------------------------------------

/// What an ADTS header needs from the AudioSpecificConfig.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// MPEG-4 audio object type of the core (2 = AAC LC).
    pub object_type: u8,
    pub sf_index: u8,
    pub channels: u8,
}

fn object_type(b: &mut Bits) -> Option<u8> {
    let t = b.read(5)?;
    Some(if t == 31 { 32 + b.read(6)? } else { t } as u8)
}

fn sf_index(b: &mut Bits) -> Option<u8> {
    let i = b.read(4)? as u8;
    if i == 0x0F {
        // Explicit frequency: ADTS cannot express it.
        b.skip(24)?;
        return None;
    }
    Some(i)
}

/// Parse an AudioSpecificConfig, consuming exactly its bits for the object
/// types broadcast uses (AAC LC, optionally signalled through SBR/PS).
fn audio_specific_config(b: &mut Bits) -> Option<AacConfig> {
    let mut aot = object_type(b)?;
    let sfi = sf_index(b)?;
    let channels = b.read(4)? as u8;
    if aot == 5 || aot == 29 {
        // Explicit SBR/PS signalling: the core config follows.
        sf_index(b)?;
        aot = object_type(b)?;
    }
    match aot {
        1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23 => {
            // GASpecificConfig
            b.read(1)?; // frameLengthFlag
            if b.read(1)? == 1 {
                b.read(14)?; // coreCoderDelay
            }
            let extension = b.read(1)?;
            if channels == 0 {
                return None; // program_config_element: not expressible in ADTS
            }
            if aot == 6 || aot == 20 {
                b.read(3)?; // layerNr
            }
            if extension == 1 {
                if aot == 22 {
                    b.read(5)?;
                    b.read(11)?;
                }
                if matches!(aot, 17 | 19 | 20 | 23) {
                    b.read(3)?;
                }
                b.read(1)?; // extensionFlag3
            }
        }
        _ => return None,
    }
    // ADTS: profile is 2 bits (object type - 1), channel_configuration 3 bits.
    if !(1..=4).contains(&aot) || channels > 7 {
        return None;
    }
    Some(AacConfig {
        object_type: aot,
        sf_index: sfi,
        channels,
    })
}

// --- StreamMuxConfig / AudioMuxElement ---------------------------------------

#[derive(Clone, Copy, Debug)]
struct MuxConfig {
    aac: AacConfig,
    num_sub_frames: u32,
    other_data_bits: Option<u32>, // Some(n) for audioMuxVersion 1, Some(0)/None otherwise
    other_data_present: bool,
}

fn stream_mux_config(b: &mut Bits) -> Option<MuxConfig> {
    let version = b.read(1)?;
    let version_a = if version == 1 { b.read(1)? } else { 0 };
    if version_a != 0 {
        return None;
    }
    if version == 1 {
        b.latm_value()?; // taraBufferFullness
    }
    let all_same_framing = b.read(1)?;
    let num_sub_frames = b.read(6)?;
    let num_program = b.read(4)?;
    let num_layer = b.read(3)?;
    if all_same_framing != 1 || num_program != 0 || num_layer != 0 {
        return None; // one program, one layer: all broadcast uses
    }
    let aac = if version == 0 {
        audio_specific_config(b)?
    } else {
        let asc_len = b.latm_value()? as usize;
        let start = b.pos;
        let cfg = audio_specific_config(b)?;
        let used = b.pos - start;
        if used > asc_len {
            return None;
        }
        b.skip(asc_len - used)?;
        cfg
    };
    let frame_length_type = b.read(3)?;
    if frame_length_type != 0 {
        return None;
    }
    b.read(8)?; // latmBufferFullness
    let other_data_present = b.read(1)? == 1;
    let mut other_data_bits = None;
    if other_data_present {
        if version == 1 {
            other_data_bits = Some(b.latm_value()?);
        } else {
            let mut len = 0u32;
            loop {
                let esc = b.read(1)?;
                len = (len << 8) + b.read(8)?;
                if esc == 0 {
                    break;
                }
            }
            other_data_bits = Some(len);
        }
    }
    if b.read(1)? == 1 {
        b.read(8)?; // crcCheckSum
    }
    Some(MuxConfig {
        aac,
        num_sub_frames,
        other_data_bits,
        other_data_present,
    })
}

pub fn adts_header(cfg: AacConfig, payload_len: usize) -> Option<[u8; 7]> {
    let len = payload_len + 7;
    if len > 0x1FFF {
        return None;
    }
    let profile = cfg.object_type - 1;
    let mut h = [0u8; 7];
    h[0] = 0xFF;
    h[1] = 0xF1; // sync, MPEG-4, layer 0, protection_absent
    h[2] = (profile << 6) | (cfg.sf_index << 2) | ((cfg.channels >> 2) & 1);
    h[3] = ((cfg.channels & 3) << 6) | ((len >> 11) as u8 & 3);
    h[4] = (len >> 3) as u8;
    h[5] = (((len & 7) as u8) << 5) | 0x1F; // buffer fullness 0x7FF (VBR)
    h[6] = 0xFC; // fullness low bits, one raw data block
    Some(h)
}

/// Stateful LOAS -> ADTS converter. The StreamMuxConfig can be sent once and
/// then referenced with useSameStreamMux, so it is remembered across calls.
#[derive(Default)]
pub struct LatmToAdts {
    config: Option<MuxConfig>,
    pub frames: u64,
    pub errors: u64,
}

impl LatmToAdts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convert one PES payload (a run of AudioSyncStream frames). Returns
    /// None if nothing could be converted.
    pub fn convert(&mut self, loas: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(loas.len() + 32);
        let mut pos = 0usize;
        let mut any = false;
        while pos + 3 <= loas.len() {
            if loas[pos] != 0x56 || loas[pos + 1] & 0xE0 != 0xE0 {
                // Lost sync inside the PES: look for the next syncword.
                pos += 1;
                continue;
            }
            let len = (usize::from(loas[pos + 1] & 0x1F) << 8) | usize::from(loas[pos + 2]);
            let end = pos + 3 + len;
            if end > loas.len() {
                self.errors += 1;
                break;
            }
            match self.mux_element(&loas[pos + 3..end], &mut out) {
                Some(()) => any = true,
                None => self.errors += 1,
            }
            pos = end;
        }
        any.then_some(out)
    }

    fn mux_element(&mut self, data: &[u8], out: &mut Vec<u8>) -> Option<()> {
        let mut b = Bits::new(data);
        if b.read(1)? == 0 {
            self.config = Some(stream_mux_config(&mut b)?);
        }
        let cfg = self.config?;
        for _ in 0..=cfg.num_sub_frames {
            // PayloadLengthInfo, frameLengthType 0
            let mut len = 0usize;
            loop {
                let t = b.read(8)? as usize;
                len += t;
                if t != 255 {
                    break;
                }
            }
            let mut frame = Vec::with_capacity(len);
            for _ in 0..len {
                frame.push(b.read(8)? as u8);
            }
            out.extend_from_slice(&adts_header(cfg.aac, len)?);
            out.extend_from_slice(&frame);
            self.frames += 1;
        }
        if cfg.other_data_present {
            if let Some(bits) = cfg.other_data_bits {
                b.skip(bits as usize)?;
            }
        }
        b.align();
        Some(())
    }
}

// --- MPEG-2 TS plumbing -----------------------------------------------------

fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// Rewrite LATM stream_type entries in a PMT packet to ADTS, in place.
/// Returns true if the packet was changed.
pub fn rewrite_pmt(pkt: &mut [u8]) -> bool {
    if pkt.len() != TS || pkt[1] & 0x40 == 0 {
        return false;
    }
    let mut p = 4usize;
    match (pkt[3] >> 4) & 3 {
        1 => {}
        3 => p += 1 + usize::from(pkt[4]),
        _ => return false,
    }
    if p >= TS {
        return false;
    }
    p += 1 + usize::from(pkt[p]); // pointer_field
    if p + 12 > TS || pkt[p] != 0x02 {
        return false;
    }
    let section_len = ((usize::from(pkt[p + 1]) & 0x0F) << 8) | usize::from(pkt[p + 2]);
    let end = p + 3 + section_len;
    if end > TS || section_len < 13 {
        return false; // multi-packet PMT: never seen here, leave it alone
    }
    let pil = ((usize::from(pkt[p + 10]) & 0x0F) << 8) | usize::from(pkt[p + 11]);
    let mut i = p + 12 + pil;
    let mut changed = false;
    while i + 5 <= end - 4 {
        if pkt[i] == STREAM_TYPE_LATM {
            pkt[i] = STREAM_TYPE_ADTS;
            changed = true;
        }
        let es_len = ((usize::from(pkt[i + 3]) & 0x0F) << 8) | usize::from(pkt[i + 4]);
        i += 5 + es_len;
    }
    if changed {
        let crc = crc32_mpeg(&pkt[p..end - 4]);
        pkt[end - 4..end].copy_from_slice(&crc.to_be_bytes());
    }
    changed
}

fn payload(pkt: &[u8]) -> Option<&[u8]> {
    let mut p = 4usize;
    match (pkt[3] >> 4) & 3 {
        1 => {}
        3 => p += 1 + usize::from(pkt[4]),
        _ => return None,
    }
    (p <= TS).then(|| &pkt[p..])
}

/// Re-packetizes the audio PID with ADTS payloads and fixes the PMT.
/// Every other packet passes through untouched and in order.
pub struct AdtsRewriter {
    conv: LatmToAdts,
    pes: Vec<u8>,
    collecting: bool,
    cc: u8,
    pub pes_converted: u64,
    pub pes_passed: u64,
    warned: bool,
}

impl Default for AdtsRewriter {
    fn default() -> Self {
        Self::new()
    }
}

impl AdtsRewriter {
    pub fn new() -> Self {
        AdtsRewriter {
            conv: LatmToAdts::new(),
            pes: Vec::with_capacity(8 * 1024),
            collecting: false,
            cc: 0,
            pes_converted: 0,
            pes_passed: 0,
            warned: false,
        }
    }

    pub fn frames(&self) -> u64 {
        self.conv.frames
    }

    pub fn errors(&self) -> u64 {
        self.conv.errors
    }

    /// Process whole 188-byte packets from `input`, appending to `out`.
    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for pkt in input.chunks_exact(TS) {
            let pid = (u32::from(pkt[1] & 0x1F) << 8) | u32::from(pkt[2]);
            if pid == PMT_PID {
                let mut p = [0u8; TS];
                p.copy_from_slice(pkt);
                rewrite_pmt(&mut p);
                out.extend_from_slice(&p);
            } else if pid == AUDIO_PID {
                self.audio_packet(pkt, out);
            } else {
                out.extend_from_slice(pkt);
            }
        }
    }

    fn audio_packet(&mut self, pkt: &[u8], out: &mut Vec<u8>) {
        let Some(data) = payload(pkt) else { return };
        if pkt[1] & 0x40 != 0 {
            if self.collecting && !self.pes.is_empty() {
                self.flush(out);
            }
            self.pes.clear();
            self.collecting = true;
        }
        if !self.collecting {
            return; // joined mid-PES
        }
        self.pes.extend_from_slice(data);
        // Flush as soon as a sized PES is complete instead of waiting for
        // the next one, so audio is not held back by a frame.
        if self.pes.len() >= 6 {
            let declared = (usize::from(self.pes[4]) << 8) | usize::from(self.pes[5]);
            if declared != 0 && self.pes.len() >= 6 + declared {
                self.pes.truncate(6 + declared);
                self.flush(out);
                self.pes.clear();
                self.collecting = false;
            }
        }
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        let pes = std::mem::take(&mut self.pes);
        let new = self.rebuild(&pes);
        let bytes = match &new {
            Some(v) => {
                self.pes_converted += 1;
                v.as_slice()
            }
            None => {
                self.pes_passed += 1;
                if !self.warned {
                    self.warned = true;
                    eprintln!(
                        "mkra: warning: audio PES not convertible to ADTS; passing LATM through"
                    );
                }
                pes.as_slice()
            }
        };
        self.packetize(bytes, out);
        self.pes = pes;
    }

    fn rebuild(&mut self, pes: &[u8]) -> Option<Vec<u8>> {
        if pes.len() < 9 || pes[0..3] != [0, 0, 1] {
            return None;
        }
        let header_end = 9 + usize::from(pes[8]);
        if header_end > pes.len() {
            return None;
        }
        let adts = self.conv.convert(&pes[header_end..])?;
        let mut v = Vec::with_capacity(header_end + adts.len());
        v.extend_from_slice(&pes[..header_end]);
        v.extend_from_slice(&adts);
        let len = v.len() - 6;
        let len = if len > 0xFFFF { 0 } else { len as u16 };
        v[4..6].copy_from_slice(&len.to_be_bytes());
        Some(v)
    }

    fn packetize(&mut self, pes: &[u8], out: &mut Vec<u8>) {
        let pid = AUDIO_PID as u16;
        let mut rest = pes;
        let mut first = true;
        while !rest.is_empty() {
            let mut p = [0xFFu8; TS];
            p[0] = 0x47;
            p[1] = ((pid >> 8) as u8 & 0x1F) | if first { 0x40 } else { 0 };
            p[2] = pid as u8;
            let n = rest.len().min(184);
            if n == 184 {
                p[3] = 0x10 | self.cc;
                p[4..].copy_from_slice(&rest[..184]);
            } else {
                p[3] = 0x30 | self.cc;
                let af_len = 183 - n;
                p[4] = af_len as u8;
                if af_len > 0 {
                    p[5] = 0x00; // no flags; the rest stays 0xFF stuffing
                }
                p[TS - n..].copy_from_slice(&rest[..n]);
            }
            self.cc = (self.cc + 1) & 0x0F;
            out.extend_from_slice(&p);
            rest = &rest[n..];
            first = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit writer for building LATM test vectors.
    struct W {
        v: Vec<u8>,
        bit: usize,
    }
    impl W {
        fn new() -> Self {
            W { v: Vec::new(), bit: 0 }
        }
        fn put(&mut self, n: u32, val: u32) {
            for i in (0..n).rev() {
                if self.bit % 8 == 0 {
                    self.v.push(0);
                }
                let b = (val >> i) & 1;
                let last = self.v.len() - 1;
                self.v[last] |= (b as u8) << (7 - self.bit % 8);
                self.bit += 1;
            }
        }
    }

    /// One AudioMuxElement (muxConfigPresent) carrying `frame`, AAC LC 48 kHz
    /// stereo, audioMuxVersion 0 — the layout the REMUX produces.
    fn element(frame: &[u8], same_mux: bool) -> Vec<u8> {
        let mut w = W::new();
        w.put(1, same_mux as u32); // useSameStreamMux
        if !same_mux {
            w.put(1, 0); // audioMuxVersion
            w.put(1, 1); // allStreamsSameTimeFraming
            w.put(6, 0); // numSubFrames
            w.put(4, 0); // numProgram
            w.put(3, 0); // numLayer
            w.put(5, 2); // AAC LC
            w.put(4, 3); // 48000
            w.put(4, 2); // stereo
            w.put(3, 0); // GASpecificConfig: frameLength, dependsOnCore, extension
            w.put(3, 0); // frameLengthType
            w.put(8, 0xFF); // latmBufferFullness
            w.put(1, 0); // otherDataPresent
            w.put(1, 0); // crcCheckPresent
        }
        let mut len = frame.len();
        while len >= 255 {
            w.put(8, 255);
            len -= 255;
        }
        w.put(8, len as u32);
        for &b in frame {
            w.put(8, u32::from(b));
        }
        let body = w.v;
        let mut loas = vec![0x56, 0xE0 | (body.len() >> 8) as u8, body.len() as u8];
        loas.extend_from_slice(&body);
        loas
    }

    #[test]
    fn adts_header_matches_ffmpeg_layout() {
        let cfg = AacConfig { object_type: 2, sf_index: 3, channels: 2 };
        let h = adts_header(cfg, 100).unwrap();
        // ffmpeg: FF F1 4C 80 0D 7F FC for LC/48k/stereo, frame 107 bytes
        assert_eq!(h, [0xFF, 0xF1, 0x4C, 0x80, 0x0D, 0x7F, 0xFC]);
    }

    #[test]
    fn converts_one_frame_and_remembers_the_config() {
        let frame: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let mut c = LatmToAdts::new();
        let mut loas = element(&frame, false);
        loas.extend(element(&frame[..10], true));
        let out = c.convert(&loas).unwrap();
        assert_eq!(c.frames, 2);
        assert_eq!(c.errors, 0);
        assert_eq!(&out[..2], &[0xFF, 0xF1]);
        assert_eq!(&out[7..307], &frame[..]);
        assert_eq!(&out[307..309], &[0xFF, 0xF1]);
        assert_eq!(&out[314..], &frame[..10]);
    }

    #[test]
    fn same_mux_without_a_config_is_an_error() {
        let mut c = LatmToAdts::new();
        assert!(c.convert(&element(&[1, 2, 3], true)).is_none());
        assert_eq!(c.errors, 1);
    }

    fn pmt_packet() -> [u8; TS] {
        let mut p = [0xFFu8; TS];
        p[..5].copy_from_slice(&[0x47, 0x41, 0x00, 0x10, 0x00]);
        let body: Vec<u8> = vec![
            0x02, 0xB0, 0x17, 0x00, 0x8D, 0xC1, 0x00, 0x00, 0xF0, 0x01, 0xF0, 0x00, //
            0x24, 0xF0, 0x11, 0xF0, 0x00, //
            0x11, 0xF1, 0x00, 0xF0, 0x00,
        ];
        let crc = crc32_mpeg(&body);
        p[5..5 + body.len()].copy_from_slice(&body);
        p[5 + body.len()..5 + body.len() + 4].copy_from_slice(&crc.to_be_bytes());
        p
    }

    #[test]
    fn pmt_audio_type_becomes_adts_with_valid_crc() {
        let mut p = pmt_packet();
        assert!(rewrite_pmt(&mut p));
        assert_eq!(p[5 + 17], STREAM_TYPE_ADTS);
        assert_eq!(p[5 + 12], 0x24); // video untouched
        // CRC over the whole section including CRC is 0 when valid.
        assert_eq!(crc32_mpeg(&p[5..5 + 26]), 0);
        assert!(!rewrite_pmt(&mut p)); // idempotent
    }

    fn audio_ts(pes: &[u8]) -> Vec<u8> {
        let mut r = AdtsRewriter::new();
        let mut out = Vec::new();
        r.packetize(pes, &mut out);
        out
    }

    #[test]
    fn rewriter_converts_a_split_pes_and_keeps_pts() {
        let frame: Vec<u8> = (0..500u32).map(|i| (i * 7) as u8).collect();
        let loas = element(&frame, false);
        let mut pes = vec![0, 0, 1, 0xC0, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
        pes.extend_from_slice(&loas);
        let len = (pes.len() - 6) as u16;
        pes[4..6].copy_from_slice(&len.to_be_bytes());
        let input = audio_ts(&pes);
        assert!(input.len() >= 3 * TS);

        let mut video = [0xFFu8; TS];
        video[..4].copy_from_slice(&[0x47, 0x10, 0x11, 0x10]);
        let mut stream = video.to_vec();
        stream.extend_from_slice(&input);

        let mut r = AdtsRewriter::new();
        let mut out = Vec::new();
        r.process(&stream, &mut out);
        assert_eq!(r.pes_converted, 1);
        assert_eq!(&out[..TS], &video[..]);

        // Reassemble the rewritten PES.
        let mut got = Vec::new();
        for (i, p) in out[TS..].chunks_exact(TS).enumerate() {
            assert_eq!(p[0], 0x47);
            assert_eq!(p[3] & 0x0F, i as u8);
            got.extend_from_slice(payload(p).unwrap());
        }
        assert_eq!(&got[..14], &[0, 0, 1, 0xC0, got[4], got[5], 0x80, 0x80, 5, 0x21, 0, 1, 0, 1]);
        let declared = (usize::from(got[4]) << 8) | usize::from(got[5]);
        assert_eq!(declared, got.len() - 6);
        assert_eq!(&got[14..16], &[0xFF, 0xF1]);
        assert_eq!(&got[21..], &frame[..]);
    }
}
