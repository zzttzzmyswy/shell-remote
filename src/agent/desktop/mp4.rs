//! Fragmented MP4 (fMP4) muxer for streaming H.264 to browser MSE.
//!
//! Produces two kinds of byte sequences:
//! - `mp4_init_segment`: `ftyp` + `moov` (incl. `avcC` sample entry) — sent
//!   once per stream so the browser can initialize its `SourceBuffer`.
//! - `mp4_fragment`: one `moof` + `mdat` per encoded frame — appended to the
//!   source buffer for as long as the viewer stays connected; each fragment
//!   carries its own decode time (`tfdt`) plus sample flags so key frames are
//!   recognizable for random access.
//!
//! The stream contract is "init first, then an unlimited list of fragments".
//! A viewer that joins mid-stream needs the current init segment again — the
//! relay caches and replays it (see `relay::desktop`).

/// Parameters that describe the encoded stream (resolved from SPS/PPS).
#[derive(Clone, Debug)]
pub struct Mp4Config {
    pub width: u32,
    pub height: u32,
    /// Nominal frame rate (frame duration in timescale units = 1000 / fps).
    pub fps: f64,
    /// Bare SPS NAL (no start code, no length prefix).
    pub sps: Vec<u8>,
    /// Bare PPS NAL (no start code, no length prefix).
    pub pps: Vec<u8>,
}

impl Mp4Config {
    /// The `avc1.xxxxxx` codec string the browser uses to create its source
    /// buffer. `profile_idc` / `constraint` / `level_idc` come from the SPS.
    pub fn codec_string(&self) -> String {
        let (profile, compat, level) = if self.sps.len() >= 4 {
            (self.sps[1], self.sps[2], self.sps[3])
        } else {
            (0x64, 0x00, 0x1f) // baseline-ish fallback
        };
        format!("avc1.{:02X}{:02X}{:02X}", profile, compat, level)
    }
}

fn u32b(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn u16b(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

/// Wrap `payload` in `[size][name]` (payload must already include the 4-byte
/// full-box version/flags where applicable).
fn box_of(name: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&u32b(8 + payload.len() as u32));
    out.extend_from_slice(name);
    out.extend_from_slice(payload);
    out
}

fn full_box(name: &[u8; 4], version: u8, flags: [u8; 3], payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + payload.len());
    inner.push(version);
    inner.extend_from_slice(&flags);
    inner.extend_from_slice(payload);
    box_of(name, &inner)
}

fn mvhd(timescale: u32) -> Vec<u8> {
    let mut matrix = [0u8; 36];
    matrix[0..4].copy_from_slice(&u32b(0x00010000));
    matrix[16..20].copy_from_slice(&u32b(0x00010000));
    matrix[32..36].copy_from_slice(&u32b(0x40000000));
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(0)); // creation
    p.extend_from_slice(&u32b(0)); // modification
    p.extend_from_slice(&u32b(timescale));
    p.extend_from_slice(&u32b(0)); // duration
    p.extend_from_slice(&u32b(0x00010000)); // rate
    p.extend_from_slice(&u16b(0x0100)); // volume
    p.extend_from_slice(&[0u8; 2]); // reserved
    p.extend_from_slice(&[0u8; 10]); // reserved
    p.extend_from_slice(&matrix);
    p.extend_from_slice(&[0u8; 24]); // predefined
    p.extend_from_slice(&u32b(2)); // next_track_id
    full_box(b"mvhd", 0, [0, 0, 0], &p)
}

fn tkhd(width: u32, height: u32) -> Vec<u8> {
    let mut matrix = [0u8; 36];
    matrix[0..4].copy_from_slice(&u32b(0x00010000));
    matrix[16..20].copy_from_slice(&u32b(0x00010000));
    matrix[32..36].copy_from_slice(&u32b(0x40000000));
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(0)); // creation
    p.extend_from_slice(&u32b(0)); // modification
    p.extend_from_slice(&u32b(1)); // track_id
    p.extend_from_slice(&[0u8; 4]); // reserved
    p.extend_from_slice(&u32b(0)); // duration
    p.extend_from_slice(&[0u8; 8]); // reserved
    p.extend_from_slice(&u16b(0)); // layer
    p.extend_from_slice(&u16b(0)); // alternate_group
    p.extend_from_slice(&u16b(0)); // volume
    p.extend_from_slice(&[0u8; 2]); // reserved
    p.extend_from_slice(&matrix);
    p.extend_from_slice(&u32b(width << 16));
    p.extend_from_slice(&u32b(height << 16));
    full_box(b"tkhd", 0, [0, 0, 0x3], &p)
}

fn mdhd(timescale: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(0)); // creation
    p.extend_from_slice(&u32b(0)); // modification
    p.extend_from_slice(&u32b(timescale));
    p.extend_from_slice(&u32b(0)); // duration
    p.extend_from_slice(&[0x55, 0xc4]); // language "und"
    p.extend_from_slice(&[0u8, 0]); // predefined
    full_box(b"mdhd", 0, [0, 0, 0], &p)
}

fn hdlr() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(0)); // pre_defined
    p.extend_from_slice(b"vide");
    p.extend_from_slice(&[0u8; 12]); // reserved
    p.extend_from_slice(b"VideoHandler\0");
    full_box(b"hdlr", 0, [0, 0, 0], &p)
}

fn vmhd() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u16b(0)); // graphicsmode
    p.extend_from_slice(&[0u8; 6]); // opcolor
    full_box(b"vmhd", 0, [0, 0, 1], &p)
}

fn dref() -> Vec<u8> {
    let url = full_box(b"url ", 0, [0, 0, 1], &[]);
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(1)); // entry_count
    p.extend_from_slice(&url);
    full_box(b"dref", 0, [0, 0, 0], &p)
}

fn avcc(cfg: &Mp4Config) -> Vec<u8> {
    assert!(!cfg.sps.is_empty() && !cfg.pps.is_empty(), "SPS/PPS required");
    let mut p = Vec::new();
    p.push(1); // configurationVersion
    p.push(cfg.sps[1]); // avcProfileIndication
    p.push(cfg.sps[2]); // profile_compatibility
    p.push(cfg.sps[3]); // avcLevelIndication
    p.push(0xff); // lengthSizeMinusOne = 3
    p.push(1); // numOfSequenceParameterSets
    p.extend_from_slice(&u16b(cfg.sps.len() as u16));
    p.extend_from_slice(&cfg.sps);
    p.push(1); // numOfPictureParameterSets
    p.extend_from_slice(&u16b(cfg.pps.len() as u16));
    p.extend_from_slice(&cfg.pps);
    full_box(b"avcC", 0, [0, 0, 0], &p)
}

fn avc1(cfg: &Mp4Config) -> Vec<u8> {
    let avcc_box = avcc(cfg);
    let mut p = Vec::new();
    p.extend_from_slice(&[0u8; 6]); // reserved
    p.extend_from_slice(&u16b(1)); // data_reference_index
    p.extend_from_slice(&u16b(0)); // pre_defined
    p.extend_from_slice(&[0u8; 2]); // reserved
    p.extend_from_slice(&[0u8; 12]); // pre_defined
    p.extend_from_slice(&u16b(cfg.width as u16));
    p.extend_from_slice(&u16b(cfg.height as u16));
    p.extend_from_slice(&u32b(0x00480000)); // horizresolution
    p.extend_from_slice(&u32b(0x00480000)); // vertresolution
    p.extend_from_slice(&u32b(0)); // reserved
    p.extend_from_slice(&u16b(1)); // frame_count
    p.extend_from_slice(&[0u8; 32]); // compressorname
    p.extend_from_slice(&u16b(24)); // depth
    p.extend_from_slice(&u16b(0xffff)); // pre_defined
    p.extend_from_slice(&avcc_box);
    box_of(b"avc1", &p)
}

fn stsd(cfg: &Mp4Config) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(1)); // entry_count
    p.extend_from_slice(&avc1(cfg));
    full_box(b"stsd", 0, [0, 0, 0], &p)
}

/// Empty `stts`/`stsc`/`stco` sample table: version/flags + entry_count=0.
fn empty_count_box(name: &[u8; 4]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(0)); // entry_count
    full_box(name, 0, [0, 0, 0], &p)
}

/// Empty `stsz`: sample_size=0 + sample_count=0 (fragmented files carry no
/// sample sizes in the moov; sizes travel in each trun).
fn empty_stsz() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(0)); // sample_size
    p.extend_from_slice(&u32b(0)); // sample_count
    full_box(b"stsz", 0, [0, 0, 0], &p)
}

/// Build the init segment: `ftyp` + `moov` with a single H.264 video track.
pub fn mp4_init_segment(cfg: &Mp4Config) -> Vec<u8> {
    let mut ftyp_payload = Vec::new();
    ftyp_payload.extend_from_slice(b"isom");
    ftyp_payload.extend_from_slice(&u32b(0)); // minor_version
    ftyp_payload.extend_from_slice(b"isomiso2avc1mp41");
    let ftyp = box_of(b"ftyp", &ftyp_payload);

    let stbl = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&stsd(cfg));
        inner.extend_from_slice(&empty_count_box(b"stts"));
        inner.extend_from_slice(&empty_count_box(b"stsc"));
        inner.extend_from_slice(&empty_stsz());
        inner.extend_from_slice(&empty_count_box(b"stco"));
        box_of(b"stbl", &inner)
    };
    let minf = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&vmhd());
        inner.extend_from_slice(&dref());
        inner.extend_from_slice(&stbl);
        box_of(b"minf", &inner)
    };
    let mdia = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&mdhd(1000));
        inner.extend_from_slice(&hdlr());
        inner.extend_from_slice(&minf);
        box_of(b"mdia", &inner)
    };
    let trak = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&tkhd(cfg.width, cfg.height));
        inner.extend_from_slice(&mdia);
        box_of(b"trak", &inner)
    };
    // mvex/trex: declares this is a fragmented MP4 and provides the default
    // sample description for every fragment track — required by MSE and
    // ffmpeg before they will accept `trun` boxes.
    let mvex = {
        let mut trex_payload = Vec::new();
        trex_payload.extend_from_slice(&u32b(1)); // track_ID
        trex_payload.extend_from_slice(&u32b(1)); // default_sample_description_index
        trex_payload.extend_from_slice(&u32b(0)); // default_sample_duration
        trex_payload.extend_from_slice(&u32b(0)); // default_sample_size
        trex_payload.extend_from_slice(&u32b(0)); // default_sample_flags
        let trex = full_box(b"trex", 0, [0, 0, 0], &trex_payload);
        box_of(b"mvex", &trex)
    };
    let moov = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&mvhd(1000));
        inner.extend_from_slice(&trak);
        inner.extend_from_slice(&mvex);
        box_of(b"moov", &inner)
    };

    let mut out = Vec::new();
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out
}

/// Frame duration in track timescale units (track timescale = 1000 = ms).
fn frame_duration(cfg: &Mp4Config) -> u32 {
    ((1000.0 / cfg.fps).round() as u32).max(1)
}

/// Build one media fragment: `moof` (mfhd + traf/tfhd+tfdt+trun) + `mdat`.
///
/// `sample` must be an AVCC sample: the frame's NAL units each prefixed with
/// a 4-byte big-endian length (no start codes). `seq` is the monotonically
/// increasing fragment sequence number.
pub fn mp4_fragment(cfg: &Mp4Config, sample: &[u8], pts_ms: u64, is_key: bool, seq: u32) -> Vec<u8> {
    let duration = frame_duration(cfg);

    // mfhd
    let mfhd = {
        let mut p = Vec::new();
        p.extend_from_slice(&u32b(seq));
        full_box(b"mfhd", 0, [0, 0, 0], &p)
    };
    // tfhd: flags = default-base-is-moof | track-id-present
    let tfhd = {
        let mut p = Vec::new();
        p.extend_from_slice(&u32b(1)); // track_ID
        full_box(b"tfhd", 0, [2, 0, 1], &p)
    };
    // tfdt: version 1, 8-byte baseMediaDecodeTime
    let tfdt = {
        let mut body = Vec::new();
        body.push(1);
        body.extend_from_slice(&[0, 0, 0]);
        body.extend_from_slice(&(pts_ms as i64).to_be_bytes());
        box_of(b"tfdt", &body)
    };
    // trun: full box (version 0) with flags =
    // data-offset-present(0x1) | first-sample-flags-present(0x4) |
    // sample-duration-present(0x100) | sample-size-present(0x200) = 0x305
    let mut trun_payload = Vec::new();
    trun_payload.push(0); // version
    trun_payload.extend_from_slice(&[0x00, 0x03, 0x05]); // flags
    trun_payload.extend_from_slice(&u32b(1)); // sample_count
    let data_offset_pos = trun_payload.len() as u32;
    trun_payload.extend_from_slice(&u32b(0)); // placeholder data_offset
    trun_payload.extend_from_slice(&u32b(if is_key { 0x02000000 } else { 0x01000000 }));
    trun_payload.extend_from_slice(&u32b(duration));
    trun_payload.extend_from_slice(&u32b(sample.len() as u32));
    let trun = box_of(b"trun", &trun_payload);

    let traf = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&tfhd);
        inner.extend_from_slice(&tfdt);
        inner.extend_from_slice(&trun);
        box_of(b"traf", &inner)
    };

    let moof = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&mfhd);
        inner.extend_from_slice(&traf);
        box_of(b"moof", &inner)
    };

    let mut mdat = Vec::with_capacity(8 + sample.len());
    mdat.extend_from_slice(&u32b(8 + sample.len() as u32));
    mdat.extend_from_slice(b"mdat");
    mdat.extend_from_slice(sample);

    // Patch the trun data_offset: offset from the very start of `moof` to the
    // first sample byte in `mdat`. The trun payload begins `moof_len` bytes
    // into the buffer and `data_offset` lives 4 + 4 bytes after the trun
    // version/flags; absolute position = moof start + mfhd+? — compute via the
    // running box walk instead of guessing.
    let data_offset = moof.len() as u32 + 8; // moof size + mdat header size
    let field_abs = find_trun_data_offset(&moof);
    let mut out = moof;
    out[field_abs..field_abs + 4].copy_from_slice(&u32b(data_offset));
    out.extend_from_slice(&mdat);
    let _ = data_offset_pos;
    out
}

/// Locate the byte position of the `trun` box's `data_offset` field inside a
/// `moof` byte string (walking nested boxes).
fn find_trun_data_offset(moof: &[u8]) -> usize {
    let mut pos = 0usize;
    while pos + 8 <= moof.len() {
        let size = u32::from_be_bytes([moof[pos], moof[pos + 1], moof[pos + 2], moof[pos + 3]]) as usize;
        let name = &moof[pos + 4..pos + 8];
        if name == b"moof" || name == b"traf" {
            // Containers have no version/flags; children start right after
            // the 8-byte box header.
            pos += 8;
            continue;
        }
        if name == b"trun" {
            // trun payload: version/flags (4) + sample_count (4) + data_offset (4)
            return pos + 8 + 4 + 4;
        }
        if size < 8 {
            break;
        }
        pos += size;
    }
    usize::MAX
}

/// Convert an Annex-B byte stream (start-code separated NAL units) into an
/// AVCC sample: each NAL prefixed with its 4-byte big-endian length. The
/// fMP4 `mdat` layout requires length prefixes, not start codes.
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    // Locate every start code (00 00 01).
    let n = annexb.len();
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 2 < n {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
            starts.push(i);
            if i + 3 < n && annexb[i + 3] == 0 {
                i += 4;
            } else {
                i += 3;
            }
        } else {
            i += 1;
        }
    }

    let mut out = Vec::with_capacity(annexb.len());
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(n);
        let mut nal = &annexb[s..e];
        // Strip the start code prefix (3 or 4 bytes).
        if nal.len() >= 4 && nal[0] == 0 && nal[1] == 0 && nal[2] == 0 && nal[3] == 1 {
            nal = &nal[4..];
        } else if nal.len() >= 3 && nal[0] == 0 && nal[1] == 0 && nal[2] == 1 {
            nal = &nal[3..];
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Mp4Config {
        Mp4Config {
            width: 320,
            height: 240,
            fps: 15.0,
            sps: vec![0x67, 0x42, 0x00, 0x1f, 0xe5, 0x01, 0x40],
            pps: vec![0x68, 0xce, 0x3c, 0x80],
        }
    }

    fn box_total(data: &[u8], mut pos: usize) -> u32 {
        let mut total = 0u32;
        while pos + 8 <= data.len() {
            let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
            if size < 8 {
                break;
            }
            total += size as u32;
            pos += size;
        }
        total
    }

    #[test]
    fn test_init_segment_contains_boxes() {
        let init = mp4_init_segment(&cfg());
        assert_eq!(&init[4..8], b"ftyp");
        assert!(init.windows(8).any(|w| &w[4..8] == b"moov"));
        assert!(init.windows(4).any(|w| w == b"avcC"));
        assert!(init.windows(4).any(|w| w == b"stsd"));
        assert!(init.windows(4).any(|w| w == b"mvex"));
        assert!(init.windows(4).any(|w| w == b"trex"));
        assert_eq!(init.len() as u32, box_total(&init, 0));
    }

    #[test]
    fn test_codec_string() {
        let c = cfg();
        assert_eq!(c.codec_string(), "avc1.42001F");
    }

    #[test]
    fn test_fragment_structure_and_trun_offset() {
        let cfg = cfg();
        let sample = annexb_to_avcc(&b"\x00\x00\x00\x01\x65\x88\x84\x01\x41\x00\x00\x00\x01\x67\x42"[..]);
        let frag = mp4_fragment(&cfg, &sample, 33, true, 1);
        assert_eq!(&frag[4..8], b"moof");
        assert_eq!(&frag[12..16], b"mfhd");
        assert!(frag.windows(4).any(|w| w == b"traf"));
        let mdat_pos = frag.windows(8).position(|w| &w[4..8] == b"mdat").unwrap();
        assert!(mdat_pos > 0);

        let field = find_trun_data_offset(&frag);
        let moof_len = u32::from_be_bytes([frag[0], frag[1], frag[2], frag[3]]) as u32;
        let expected = moof_len + 8;
        let actual = u32::from_be_bytes([frag[field], frag[field + 1], frag[field + 2], frag[field + 3]]);
        assert_eq!(actual, expected, "trun data_offset must point into mdat payload");
    }

    #[test]
    fn test_annexb_to_avcc_conversion() {
        let annexb = b"\x00\x00\x00\x01\x65\x01\x02\x00\x00\x01\x41\x03\x04";
        let avcc = annexb_to_avcc(annexb);
        assert_eq!(avcc.len(), 4 + 3 + 4 + 3);
        assert_eq!(&avcc[..4], &[0, 0, 0, 3]);
        assert_eq!(&avcc[4..7], &[0x65, 0x01, 0x02]);
        assert_eq!(&avcc[7..11], &[0, 0, 0, 3]);
        assert_eq!(&avcc[11..14], &[0x41, 0x03, 0x04]);
    }

    #[test]
    fn test_frame_duration_15fps() {
        assert_eq!(frame_duration(&cfg()), 67);
    }
}

