//! Fragmented MP4 (fMP4) muxer for streaming H.264 / VP9 to browser MSE.
//!
//! Produces two kinds of byte sequences:
//! - `mp4_init_segment`: `ftyp` + `moov` (incl. `avcC`/`vpcC` sample entry) —
//!   sent once per stream so the browser can initialize its `SourceBuffer`.
//! - `mp4_fragment`: one `moof` + `mdat` per encoded frame — appended to the
//!   source buffer for as long as the viewer stays connected; each fragment
//!   carries its own decode time (`tfdt`) plus sample flags so key frames are
//!   recognizable for random access.
//!
//! The stream contract is "init first, then an unlimited list of fragments".
//! A viewer that joins mid-stream needs the current init segment again — the
//! relay caches and replays it (see `relay::desktop`).

/// 视频采样描述：codec 参数集（H.264 走 avcC；VP9 走 vpcC；AV1 走 av1C）。
#[derive(Clone, Debug)]
pub enum VisualSample {
    /// H.264: bare SPS/PPS NAL (no start code, no length prefix), carried in
    /// the `avcC` box.
    H264 { sps: Vec<u8>, pps: Vec<u8> },
    /// VP9: profile_idc / level_idc, carried in the `vpcC` box.
    Vp9 { profile: u8, level: u8 },
    /// AV1: profile / level (level is AV1 level_idx, carried in `av1C`).
    Av1 { profile: u8, level: u8 },
}

/// Parameters that describe the encoded stream (resolved from SPS/PPS or VP9
/// config).
#[derive(Clone, Debug)]
pub struct Mp4Config {
    pub width: u32,
    pub height: u32,
    /// Nominal frame rate (frame duration in timescale units = 1000 / fps).
    pub fps: f64,
    /// Sample description (codec parameter set).
    pub sample: VisualSample,
}

impl Mp4Config {
    /// The codec string the browser uses to create its source buffer /
    /// WebCodecs decoder. H.264 → `avc1.xxxxxx`（取自 SPS）; VP9 →
    /// `vp09.PP.LL.DD`（profile/level/bitdepth）。
    pub fn codec_string(&self) -> String {
        match &self.sample {
            VisualSample::H264 { sps, .. } => {
                let (profile, compat, level) = if sps.len() >= 4 {
                    (sps[1], sps[2], sps[3])
                } else {
                    (0x64, 0x00, 0x1f) // baseline-ish fallback
                };
                format!("avc1.{:02X}{:02X}{:02X}", profile, compat, level)
            }
            VisualSample::Vp9 { profile, level } => {
                format!("vp09.{:02}.{:02}.08", profile, level)
            }
            VisualSample::Av1 { profile, level } => {
                // AV1 codec string: av01.P.LLT.DD；P=profile, LL=seq_level_idx
                // 的十进制两位（3.0→idx2→"02", 4.0→idx4→"04"），Chrome 按
                // 5 位 idx(0-31) 校验, 写两位 level 号(30/40)会被拒。
                format!("av01.{}.{:02}M.08", profile, av1_level_to_idx(*level))
            }
        }
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
    p.extend_from_slice(&[0u8; 8]); // reserved
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
    let (sps, pps) = match &cfg.sample {
        VisualSample::H264 { sps, pps } => (sps, pps),
        VisualSample::Vp9 { .. } | VisualSample::Av1 { .. } => unreachable!("avcC is h264-only"),
    };
    assert!(!sps.is_empty() && !pps.is_empty(), "SPS/PPS required");
    let mut p = Vec::new();
    p.push(1); // configurationVersion
    p.push(sps[1]); // avcProfileIndication
    p.push(sps[2]); // profile_compatibility
    p.push(sps[3]); // avcLevelIndication
    p.push(0xff); // lengthSizeMinusOne = 3
    p.push(1); // numOfSequenceParameterSets
    p.extend_from_slice(&u16b(sps.len() as u16));
    p.extend_from_slice(sps);
    p.push(1); // numOfPictureParameterSets
    p.extend_from_slice(&u16b(pps.len() as u16));
    p.extend_from_slice(pps);
    // avcC 是普通 box（不含 version/flags），勿用 full_box。
    box_of(b"avcC", &p)
}

/// VP9 codec configuration record (`vpcC`). 8-bit 4:2:0, BT.709.
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

fn vpcc(profile: u8, level: u8) -> Vec<u8> {
    // version/flags (full box, version 1) then:
    //   profile(1) level(1) bitDepth(4) chromaSubsampling(3) videoFullRange(1)
    //   colourPrimaries(1) transferCharacteristics(1) matrixCoefficients(1)
    //   codecInitializationDataSize(2)
    let mut p = Vec::new();
    p.push(1); // full box version
    p.extend_from_slice(&[0, 0, 0]); // flags
    p.push(profile);
    p.push(level);
    p.push((8u8 << 4) | (1u8 << 1)); // bitDepth=8, chromaSubsampling=1 (4:2:0)
    p.push(1); // colourPrimaries = BT.709
    p.push(1); // transferCharacteristics = BT.709
    p.push(1); // matrixCoefficients = BT.709
    p.extend_from_slice(&u16b(0)); // codecInitializationDataSize
    box_of(b"vpcC", &p)
}

fn sample_entry(cfg: &Mp4Config) -> Vec<u8> {
    match &cfg.sample {
        VisualSample::H264 { .. } => avc1(cfg),
        VisualSample::Vp9 { profile, level } => vp09(cfg, *profile, *level),
        VisualSample::Av1 { profile, level } => av01(cfg, *profile, *level),
    }
}

/// AV1 两位 level（30=3.0、40=4.0…）→ av1C `seq_level_idx_0`（5 位字段）。
/// AV1 规范映射: 2.0→0, 2.1→1, 3.0→2, 3.1→3, 4.0→4, 4.1→5, 5.0→6,
/// 5.1→7, 6.0→8, 6.1→9, 6.2→10, 6.3→11。直接 `level & 0x1f` 在 ≥4.0
/// (40&0x1f=8) 时会把错误的索引写进 box。
fn av1_level_to_idx(level: u8) -> u8 {
    let major = level / 10;
    let minor = level % 10;
    match major {
        2 => minor,
        3 => 2 + minor,
        4 => 4 + minor,
        5 => 6 + minor,
        6 => 8 + minor,
        _ => 0,
    }
}

/// AV1 codec configuration record (`av1C`), per ISO/IEC 14496-15 §8.x
/// (AV1CodecConfigurationRecord — 普通 box, 无 version/flags)。布局:
///   marker(1)+version(7)=0x81
///   seq_profile(3) seq_level_idx_0(5)
///   seq_tier_0(1) high_bitdepth(1) twelve_bit(1) monochrome(1)
///     chroma_subsampling_x(1) chroma_subsampling_y(1) chroma_sample_position(2)
///   reserved(3) initial_presentation_delay_present(1) reserved(4)
fn av1c(profile: u8, level: u8) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x81); // marker=1, version=1
    p.push((profile << 5) | av1_level_to_idx(level)); // profile + level_idx
    // 8-bit 4:2:0: tier=0 high=0 twelve=0 mono=0 x=1 y=1 pos=0
    p.push(0b0000_1100);
    p.push(0); // no initial_presentation_delay
    box_of(b"av1C", &p)
}

fn av01(cfg: &Mp4Config, profile: u8, level: u8) -> Vec<u8> {
    let av1c_box = av1c(profile, level);
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
    p.extend_from_slice(&av1c_box);
    box_of(b"av01", &p)
}

fn vp09(cfg: &Mp4Config, profile: u8, level: u8) -> Vec<u8> {
    let vpcc_box = vpcc(profile, level);
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
    p.extend_from_slice(&vpcc_box);
    box_of(b"vp09", &p)
}

fn stsd(cfg: &Mp4Config) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u32b(1)); // entry_count
    p.extend_from_slice(&sample_entry(cfg));
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

/// Build one media fragment: `moof` (mfhd + traf/tfhd+tfdt+trun+srtc) + `mdat`.
///
/// `sample` must be an AVCC sample: the frame's NAL units each prefixed with
/// a 4-byte big-endian length (no start codes). `seq` is the monotonically
/// increasing fragment sequence number. `capture_epoch_ms` is the frame's
/// wall-clock capture timestamp; it rides a custom `srtc` box inside `traf`
/// (ISO-BMFF mandates unknown boxes be skipped, so MSE/ffmpeg ignore it)
/// letting the browser compute true end-to-end latency on the WebCodecs path.
pub fn mp4_fragment(
    cfg: &Mp4Config,
    sample: &[u8],
    pts_ms: u64,
    is_key: bool,
    seq: u32,
    capture_epoch_ms: u64,
) -> Vec<u8> {
    let duration = frame_duration(cfg);

    // mfhd
    let mfhd = {
        let mut p = Vec::new();
        p.extend_from_slice(&u32b(seq));
        full_box(b"mfhd", 0, [0, 0, 0], &p)
    };
    // tfhd: flags = default-base-is-moof(0x020000)。注意不能带 0x1 —— ffmpeg
    // 把 0x1 解释为 base-data-offset-present 并强制再读 8 字节，导致 overread；
    // ISO 语义则 0x1=track-id-present。track_ID 在两种解析器里都是无条件读取，
    // 因此只置 0x020000 即可让 data_offset 相对 moof 起点。
    let tfhd = {
        let mut p = Vec::new();
        p.extend_from_slice(&u32b(1)); // track_ID
        full_box(b"tfhd", 0, [2, 0, 0], &p)
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

    // srtc (screen remote capture time, custom box): 8-byte capture epoch ms.
    // Browsers parse and skip unknown boxes per ISO-BMFF, so this is invisible
    // to MSE/ffmpeg but extractable by our WebCodecs player for e2e latency.
    let srtc = box_of(b"srtc", &capture_epoch_ms.to_be_bytes());

    let traf = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&tfhd);
        inner.extend_from_slice(&tfdt);
        inner.extend_from_slice(&trun);
        inner.extend_from_slice(&srtc);
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
///
/// SPS/PPS NAL units (types 7/8) are dropped — they live in the `avcC` box
/// and must not appear in-band (combined in-band+avcC parameter sets are
/// legal but poorly handled by both MSE and ffmpeg's MP4 demuxer).
/// Returns `(avcc, has_key)` where `has_key` reports whether the VCL part
/// contained an IDR slice (type 5).
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let (out, _) = annexb_to_avcc_detailed(annexb);
    out
}

/// Like [`annexb_to_avcc`] but also reports whether an IDR slice was present.
pub fn annexb_to_avcc_detailed(annexb: &[u8]) -> (Vec<u8>, bool) {
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
    let mut has_key = false;
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(n);
        let mut nal = &annexb[s..e];
        // Strip the start code prefix (3 or 4 bytes).
        if nal.len() >= 4 && nal[0] == 0 && nal[1] == 0 && nal[2] == 0 && nal[3] == 1 {
            nal = &nal[4..];
        } else if nal.len() >= 3 && nal[0] == 0 && nal[1] == 0 && nal[2] == 1 {
            nal = &nal[3..];
        }
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1f;
        // 7=SPS, 8=PPS —— 参数集不进 sample
        if nal_type == 7 || nal_type == 8 {
            continue;
        }
        if nal_type == 5 {
            has_key = true;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    (out, has_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Mp4Config {
        Mp4Config {
            width: 320,
            height: 240,
            fps: 15.0,
            sample: VisualSample::H264 {
                sps: vec![0x67, 0x42, 0x00, 0x1f, 0xe5, 0x01, 0x40],
                pps: vec![0x68, 0xce, 0x3c, 0x80],
            },
        }
    }

    fn vp9_cfg() -> Mp4Config {
        Mp4Config {
            width: 320,
            height: 240,
            fps: 15.0,
            sample: VisualSample::Vp9 { profile: 0, level: 10 },
        }
    }

    fn av1_cfg() -> Mp4Config {
        Mp4Config {
            width: 320,
            height: 240,
            fps: 15.0,
            sample: VisualSample::Av1 { profile: 0, level: 30 },
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
    fn test_vp9_init_segment_contains_vpcc() {
        let init = mp4_init_segment(&vp9_cfg());
        assert_eq!(&init[4..8], b"ftyp");
        assert!(init.windows(8).any(|w| &w[4..8] == b"moov"));
        assert!(init.windows(4).any(|w| w == b"vpcC"), "vp9 init must carry vpcC");
        assert!(init.windows(4).any(|w| w == b"vp09"), "vp9 sample entry must be vp09");
        assert_eq!(init.len() as u32, box_total(&init, 0));
    }

    #[test]
    fn test_av1_init_segment_contains_av1c() {
        let init = mp4_init_segment(&av1_cfg());
        assert_eq!(&init[4..8], b"ftyp");
        assert!(init.windows(8).any(|w| &w[4..8] == b"moov"));
        assert!(init.windows(4).any(|w| w == b"av1C"), "av1 init must carry av1C");
        assert!(init.windows(4).any(|w| w == b"av01"), "av1 sample entry must be av01");
        assert_eq!(init.len() as u32, box_total(&init, 0));
        // av1C payload: marker-version 0x81, profile<<5|level_idx, fmt byte
        let pos = init.windows(4).position(|w| w == b"av1C").unwrap();
        assert_eq!(init[pos + 4], 0x81);
        assert_eq!(init[pos + 5], (0u8 << 5) | 2u8, "profile0 level30 -> seq_level_idx 2");
        assert_eq!(init[pos + 6], 0b0000_1100, "4:2:0 8-bit");
    }

    #[test]
    fn test_codec_string() {
        let c = cfg();
        assert_eq!(c.codec_string(), "avc1.42001F");
        assert_eq!(vp9_cfg().codec_string(), "vp09.00.10.08");
        assert_eq!(av1_cfg().codec_string(), "av01.0.02M.08");
    }

    #[test]
    fn test_fragment_structure_and_trun_offset() {
        let cfg = cfg();
        let sample = annexb_to_avcc(&b"\x00\x00\x00\x01\x65\x88\x84\x01\x41\x00\x00\x00\x01\x67\x42"[..]);
        let frag = mp4_fragment(&cfg, &sample, 33, true, 1, 1_700_000_000_000);
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

        // srtc 自定义 box: 编码了 capture epoch ms, 且位于 traf 内。
        let srtc_pos = frag.windows(8).position(|w| &w[4..8] == b"srtc").unwrap();
        assert_eq!(
            u64::from_be_bytes(frag[srtc_pos + 8..srtc_pos + 16].try_into().unwrap()),
            1_700_000_000_000,
            "srtc must carry the capture epoch ms verbatim"
        );
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

