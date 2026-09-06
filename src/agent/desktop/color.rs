//! BGRA → I420 (YUV 4:2:0 planar) color conversion.
//!
//! Screen captures arrive as packed 32-bit BGRA pixels (X11 ShmImage /
//! Windows GDI BitBlt both produce B, G, R, A byte order on little-endian
//! hosts). OpenH264 consumes planar I420. The conversion is scalar
//! BT.601/BT.709 with studio-swing clamps; correctness is unit-tested
//! directly so the rest of the pipeline can trust its layout.

/// 色彩矩阵（R5#136-146 色彩矩阵最小可验证子集）：H.264 标准默认 BT.601，
/// WebM/HD 常选 BT.709（同一像素两种矩阵 Y/U/V 系数不同）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
}

/// 矩阵系数（整数定点，>>8 缩放）：`(yr, yg, yb, ur, ug, ub, vr, vg, vb)`。
/// BT.601: Y=0.299/0.587/0.114；U=-0.169/-0.331/0.500；V=0.500/-0.419/-0.081
/// BT.709: Y=0.2126/0.7152/0.0722；U=-0.1146/-0.3854/0.500；V=0.5/-0.4541/-0.0459
fn matrix_coeffs(m: ColorMatrix) -> (i32, i32, i32, i32, i32, i32, i32, i32, i32) {
    match m {
        ColorMatrix::Bt601 => (66, 129, 25, -38, -74, 112, 112, -94, -18),
        ColorMatrix::Bt709 => (54, 183, 18, -29, -99, 128, 128, -116, -12),
    }
}

/// Convert a packed BGRA buffer (4 bytes/pixel: B, G, R, A) into planar I420
/// using the given color matrix.
///
/// Returns `w*h + w*h/2` bytes: full-resolution Y plane, then quarter-size
/// U and V planes in that order. `stride` is the source row stride in bytes
/// (typically `w*4`, but X11 images may add padding).
///
/// 像素转换 SIMD（R5 #127-128 最小可验证子集）：x86-64 上运行时检测
/// SSSE3/SSE4.1，命中则 Y 平面走 128 位 SIMD 内核（4 像素/拍），U/V
/// 子采样网格行内不连续保持标量；输出与标量逐字节一致（测试强制）。
pub fn bgra_to_i420_with_matrix(
    bgra: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    matrix: ColorMatrix,
) -> Vec<u8> {
    assert!(w % 2 == 0 && h % 2 == 0, "size must be even for 4:2:0");
    assert!(bgra.len() >= h * stride, "source buffer too small");
    let (kyr, kyg, kyb, kur, kug, kub, kvr, kvg, kvb) = matrix_coeffs(matrix);
    let y_len = w * h;
    let uv_len = y_len / 4;
    let mut yuv = vec![0u8; y_len + 2 * uv_len];
    let (y_part, uv_part) = yuv.split_at_mut(y_len);
    let (u_part, v_part) = uv_part.split_at_mut(uv_len);
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse4.1") {
            unsafe {
                y_plane_simd(bgra, w, h, stride, kyr, kyg, kyb, y_part);
            }
            uv_plane_scalar(bgra, w, h, stride, kur, kug, kub, kvr, kvg, kvb, u_part, v_part);
            return yuv;
        }
    }
    y_plane_scalar(bgra, w, h, stride, kyr, kyg, kyb, y_part);
    uv_plane_scalar(bgra, w, h, stride, kur, kug, kub, kvr, kvg, kvb, u_part, v_part);
    yuv
}

/// 标量 Y 平面参考实现（SIMD 一致性基线，也作为非 x86 / 无特性回退）。
fn y_plane_scalar(
    bgra: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    kyr: i32,
    kyg: i32,
    kyb: i32,
    y_part: &mut [u8],
) {
    for row in 0..h {
        for col in 0..w {
            let src = row * stride + col * 4;
            let b = bgra[src] as i32;
            let g = bgra[src + 1] as i32;
            let r = bgra[src + 2] as i32;
            let y = ((kyr * r + kyg * g + kyb * b + 128) >> 8) + 16;
            y_part[row * w + col] = y.clamp(16, 235) as u8;
        }
    }
}

/// 标量 U/V 子采样（每 2x2 块左上像素，写 1/4 尺寸平面）。
fn uv_plane_scalar(
    bgra: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    kur: i32,
    kug: i32,
    kub: i32,
    kvr: i32,
    kvg: i32,
    kvb: i32,
    u_part: &mut [u8],
    v_part: &mut [u8],
) {
    let sw = w / 2;
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let src = row * stride + col * 4;
            let b = bgra[src] as i32;
            let g = bgra[src + 1] as i32;
            let r = bgra[src + 2] as i32;
            let u = ((kur * r + kug * g + kub * b + 128) >> 8) + 128;
            let v = ((kvr * r + kvg * g + kvb * b + 128) >> 8) + 128;
            u_part[(row / 2) * sw + col / 2] = u.clamp(16, 240) as u8;
            v_part[(row / 2) * sw + col / 2] = v.clamp(16, 240) as u8;
        }
    }
}

/// SIMD Y 平面内核（SSSE3 shuffle 提取 B/G/R 通道 + SSE4.1 i32 乘加、
/// 饱和打包）。每拍 4 像素；行尾不足 4 像素部分保持标量，保证与
/// [`y_plane_scalar`] 逐字节一致。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn y_plane_simd(
    bgra: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    kyr: i32,
    kyg: i32,
    kyb: i32,
    y_part: &mut [u8],
) {
    use std::arch::x86_64::*;
    // BGRA 布局中提取各通道字节：shuffle_epi8 遮蔽为 [ch,0,0,0, ch,0,0,0, …]
    // 后直接按 i32 视图即 [c0,c1,c2,c3]（每 4 字节一个 32bit 像素通道值）。
    let b_mask = _mm_setr_epi8(0, -1, -1, -1, 4, -1, -1, -1, 8, -1, -1, -1, 12, -1, -1, -1);
    let g_mask = _mm_setr_epi8(1, -1, -1, -1, 5, -1, -1, -1, 9, -1, -1, -1, 13, -1, -1, -1);
    let r_mask = _mm_setr_epi8(2, -1, -1, -1, 6, -1, -1, -1, 10, -1, -1, -1, 14, -1, -1, -1);
    let k_b = _mm_set1_epi32(kyb);
    let k_g = _mm_set1_epi32(kyg);
    let k_r = _mm_set1_epi32(kyr);
    let round = _mm_set1_epi32(128);
    let bias = _mm_set1_epi32(16);
    let lo = _mm_set1_epi32(16);
    let hi = _mm_set1_epi32(235);
    for row in 0..h {
        let base = row * stride;
        let row_out = row * w;
        let mut col = 0usize;
        while col + 4 <= w {
            let p = bgra.as_ptr().add(base + col * 4) as *const __m128i;
            let px = _mm_loadu_si128(p); // 4 像素 BGRA（16 字节）
            // shuffle 把每像素同通道提到字节 0/4/8/12，其余置 0 → 小端 i32 视图
            // 即 [c0,c1,c2,c3]（每 4 字节一个 32bit 像素通道值）。
            let bs = _mm_shuffle_epi8(px, b_mask); // [b0,b1,b2,b3] i32
            let gs = _mm_shuffle_epi8(px, g_mask);
            let rs = _mm_shuffle_epi8(px, r_mask);
            let sum = _mm_add_epi32(
                _mm_add_epi32(_mm_mullo_epi32(bs, k_b), _mm_mullo_epi32(gs, k_g)),
                _mm_mullo_epi32(rs, k_r),
            );
            let y = _mm_add_epi32(_mm_srai_epi32(_mm_add_epi32(sum, round), 8), bias);
            let yc = _mm_min_epi32(_mm_max_epi32(y, lo), hi);
            let packed = _mm_packus_epi32(yc, yc); // i32 → u16 饱和（16..=235 安全）
            let packed = _mm_packus_epi16(packed, packed); // u16 → u8 饱和
            let out = _mm_cvtsi128_si32(packed); // 低 4 字节 = 4 个 Y
            // 行起始可能非 4 对齐（w 非 4 倍数时 row*w 偏移），用非对齐写。
            let dst = y_part.as_mut_ptr().add(row_out + col);
            std::ptr::write_unaligned(dst as *mut u32, out as u32);
            col += 4;
        }
        // 行尾余量（col < w 且 < 4）：标量，保证与参考一致。
        while col < w {
            let src = base + col * 4;
            let b = bgra[src] as i32;
            let g = bgra[src + 1] as i32;
            let r = bgra[src + 2] as i32;
            let y = ((kyr * r + kyg * g + kyb * b + 128) >> 8) + 16;
            y_part[row_out + col] = y.clamp(16, 235) as u8;
            col += 1;
        }
    }
}

/// Convert a packed BGRA buffer into planar I420 using the default BT.601
/// matrix (H.264 / OpenH264 标准色域)。
pub fn bgra_to_i420(bgra: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
    bgra_to_i420_with_matrix(bgra, w, h, stride, ColorMatrix::Bt601)
}

/// Box-filter downscale of a BGRA frame straight into I420 at a smaller size.
///
/// `(w0,h0)` is the source capture size, `(w1,h1) ≤ (w0,h0)` the encode size.
/// Each target pixel averages its mapped source region (integer box filter, no
/// floating point). Both sizes must be even; `w1` and `h1` can be equal to
/// `w0/h0` (then this is identical to [`bgra_to_i420`]).
pub fn bgra_to_i420_scaled(
    bgra: &[u8],
    w0: usize,
    h0: usize,
    stride: usize,
    w1: usize,
    h1: usize,
) -> Vec<u8> {
    assert!(w0 % 2 == 0 && h0 % 2 == 0 && w1 % 2 == 0 && h1 % 2 == 0);
    assert!(w1 <= w0 && h1 <= h0, "upscale unsupported: {w0}x{h0} -> {w1}x{h1}");
    assert!(bgra.len() >= h0 * stride, "source buffer too small");
    let y_len = w1 * h1;
    let uv_len = y_len / 4;
    let mut yuv = vec![0u8; y_len + 2 * uv_len];
    let (y_part, uv_part) = yuv.split_at_mut(y_len);
    let (u_part, v_part) = uv_part.split_at_mut(uv_len);
    let sw1 = w1 / 2;
    for ty in 0..h1 {
        let y0 = ty * h0 / h1;
        let y1 = ((ty + 1) * h0 / h1).max(y0 + 1);
        for tx in 0..w1 {
            let x0 = tx * w0 / w1;
            let x1 = ((tx + 1) * w0 / w1).max(x0 + 1);
            let (mut r, mut g, mut b, mut n) = (0i32, 0i32, 0i32, 0u32);
            for sy in y0..y1 {
                let base = sy * stride;
                for sx in x0..x1 {
                    let src = base + sx * 4;
                    b += bgra[src] as i32;
                    g += bgra[src + 1] as i32;
                    r += bgra[src + 2] as i32;
                    n += 1;
                }
            }
            let n = n as i32;
            let r = r / n;
            let g = g / n;
            let b = b / n;
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_part[ty * w1 + tx] = y.clamp(16, 235) as u8;
            if tx % 2 == 0 && ty % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                u_part[(ty / 2) * sw1 + tx / 2] = u.clamp(16, 240) as u8;
                v_part[(ty / 2) * sw1 + tx / 2] = v.clamp(16, 240) as u8;
            }
        }
    }
    yuv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dimensions() {
        let bgra = vec![0u8; 320 * 240 * 4];
        let yuv = bgra_to_i420(&bgra, 320, 240, 320 * 4);
        assert_eq!(yuv.len(), 320 * 240 + 2 * (320 / 2) * (240 / 2));
    }

    #[test]
    fn test_color_matrix_bt709_differs_from_bt601() {
        // R5#136-146 色彩矩阵最小子集：同一彩色像素（蓝红各半 + 绿）两种
        // 矩阵的 Y/U/V 输出应不同（BT.709 与 BT.601 系数不同）。
        let mut bgra = vec![0u8; 4 * 4 * 4];
        for px in 0..16 {
            let o = px * 4;
            bgra[o] = 200;     // B
            bgra[o + 1] = 100; // G
            bgra[o + 2] = 60;  // R
            bgra[o + 3] = 255; // A
        }
        let y601 = bgra_to_i420_with_matrix(&bgra, 4, 4, 16, ColorMatrix::Bt601);
        let y709 = bgra_to_i420_with_matrix(&bgra, 4, 4, 16, ColorMatrix::Bt709);
        assert_ne!(y601, y709, "BT.601 与 BT.709 输出必须不同");
        // 默认入口 = BT.601（H.264 标准色域，回归保护）。
        assert_eq!(bgra_to_i420(&bgra, 4, 4, 16), y601);
    }

    #[test]
    fn test_1080p_memory_layout() {
        let bgra = vec![9u8; 1920 * 1080 * 4];
        let yuv = bgra_to_i420(&bgra, 1920, 1080, 1920 * 4);
        assert_eq!(yuv.len(), 1920 * 1080 + 2 * (1920 / 2) * (1080 / 2));
        // 灰阶中性色 (9,9,9) → Y > 0
        assert!(yuv[0] > 0);
    }

    #[test]
    fn test_white_pixel_y_near_max() {
        // 纯白像素 → Y=235 上限附近
        let mut bgra = vec![0u8; 4 * 4 * 4];
        for px in 0..16 {
            bgra[px * 4..px * 4 + 3].copy_from_slice(&[255, 255, 255]); // B,G,R
            bgra[px * 4 + 3] = 255;
        }
        let yuv = bgra_to_i420(&bgra, 4, 4, 16);
        assert!(yuv[0] > 200, "white Y should be bright, got {}", yuv[0]);
        assert!(yuv[0] <= 235);
    }

    #[test]
    fn test_known_gray_value() {
        // 中灰 (128,128,128) → Y ≈ 126 (BT.601: 0.299·128+0.587·128+0.114·128=128 → y=0+16=... 计算: 129*128≈16512>>8=64.5 + 16 ≈ 80 附近)
        // 这里只验证在合理范围,不锁精确值(系数近似)
        let mut bgra = vec![128u8; 4 * 4 * 4];
        bgra[3] = 255; // A
        let yuv = bgra_to_i420(&bgra, 4, 4, 16);
        let y = yuv[0] as i32;
        assert!((80..=140).contains(&y), "gray Y in plausible band, got {}", y);
    }

    #[test]
    fn test_padded_stride_respected() {
        // 源行 stride=20(16 字节像素数据 + 4 字节尾部 padding), 每像素仍取前 16 字节
        let stride = 20;
        let w = 4;
        let h = 4;
        let mut bgra = vec![0u8; stride * h];
        for row in 0..h {
            for px in 0..w {
                let i = row * stride + px * 4;
                bgra[i] = 255; // B
                bgra[i + 1] = 255; // G
                bgra[i + 2] = 255; // R
                bgra[i + 3] = 255; // A
            }
        }
        let yuv = bgra_to_i420(&bgra, w, h, stride);
        assert!(yuv[0] > 200, "padded stride frame should be white");
    }

    #[test]
    fn test_scaled_dimensions_and_value() {
        // 8x8 纯白源 → 4x4 目标, 尺寸正确且像素仍为白
        let w0 = 8;
        let h0 = 8;
        let mut bgra = vec![0u8; w0 * h0 * 4];
        for px in 0..(w0 * h0) {
            bgra[px * 4..px * 4 + 3].copy_from_slice(&[255, 255, 255]);
            bgra[px * 4 + 3] = 255;
        }
        let yuv = bgra_to_i420_scaled(&bgra, w0, h0, w0 * 4, 4, 4);
        assert_eq!(yuv.len(), 4 * 4 + 2 * (4 / 2) * (4 / 2));
        assert!(yuv[0] > 200, "downscaled white should stay white, got {}", yuv[0]);
    }

    #[test]
    fn test_scaled_averages_source_region() {
        // 2x4 源: 上半两行白, 下半两行黑 → 缩为 1x2 (每目标像素平均 2x4 区域)
        // 目标 (0,0)=白, (0,1)=黑
        let w0 = 2;
        let h0 = 4;
        let mut bgra = vec![0u8; w0 * h0 * 4];
        for row in 0..2 {
            for px in 0..w0 {
                let i = row * (w0 * 4) + px * 4;
                bgra[i] = 255;
                bgra[i + 1] = 255;
                bgra[i + 2] = 255;
                bgra[i + 3] = 255;
            }
        }
        let yuv = bgra_to_i420_scaled(&bgra, w0, h0, w0 * 4, 2, 2);
        assert!(yuv[0] > 200, "top target should be white, got {}", yuv[0]);
        let bottom = yuv[2];
        assert!(bottom < 80, "bottom target should be black, got {}", bottom);
    }

    /// 构造确定性混合 BGRA 帧（伪随机但可复现，覆盖全值域 + 尾部对齐余量）。
    fn make_bgra(w: usize, h: usize, stride: usize, seed: u64) -> Vec<u8> {
        let mut buf = vec![0u8; h * stride];
        let mut x = seed;
        for i in 0..buf.len() {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            buf[i] = (x >> 33) as u8;
        }
        buf
    }

    /// SIMD dispatch 输出与标量参考逐字节一致（R5 #127-128 SIMD 子集正确性）。
    fn assert_simd_matches_scalar(w: usize, h: usize, stride: usize, matrix: ColorMatrix, seed: u64) {
        let bgra = make_bgra(w, h, stride, seed);
        let yuv = bgra_to_i420_with_matrix(&bgra, w, h, stride, matrix);
        // 标量参考：手工按原算法重算（不经 SIMD dispatch）。
        let (kyr, kyg, kyb, kur, kug, kub, kvr, kvg, kvb) = matrix_coeffs(matrix);
        let y_len = w * h;
        let uv_len = y_len / 4;
        let mut ref_yuv = vec![0u8; y_len + 2 * uv_len];
        let (y_part, uv_part) = ref_yuv.split_at_mut(y_len);
        let (u_part, v_part) = uv_part.split_at_mut(uv_len);
        y_plane_scalar(&bgra, w, h, stride, kyr, kyg, kyb, y_part);
        uv_plane_scalar(&bgra, w, h, stride, kur, kug, kub, kvr, kvg, kvb, u_part, v_part);
        assert_eq!(yuv, ref_yuv, "SIMD 与标量输出不一致（{w}x{h} stride={stride} matrix={matrix:?}）");
    }

    #[test]
    fn test_simd_matches_scalar_bt601_padded() {
        // 紧凑 + padding stride，均应与标量逐字节一致。
        assert_simd_matches_scalar(64, 48, 64 * 4, ColorMatrix::Bt601, 0x601);
        assert_simd_matches_scalar(64, 48, 64 * 4 + 8, ColorMatrix::Bt601, 0x602);
    }

    #[test]
    fn test_simd_matches_scalar_bt709() {
        assert_simd_matches_scalar(128, 64, 128 * 4, ColorMatrix::Bt709, 0x709);
    }

    #[test]
    fn test_simd_matches_scalar_tail_margin() {
        // w=10 非 4 倍数 → 每行尾部 2 像素走标量补齐，仍逐字节一致。
        assert_simd_matches_scalar(10, 8, 10 * 4, ColorMatrix::Bt601, 0x301);
        assert_simd_matches_scalar(14, 6, 14 * 4 + 16, ColorMatrix::Bt601, 0x302);
    }

    #[test]
    fn test_simd_matches_scalar_extremes() {
        // 全白/全黑/全 A=0 极端输入。
        for (seed, fill) in [(0x11, 255u8), (0x22, 0u8), (0x33, 1u8)] {
            let w = 16;
            let h = 8;
            let stride = w * 4;
            let mut bgra = vec![fill; h * stride];
            let _ = seed;
            bgra[3] = 0; // A 通道置 0，确认 A 不影响转换
            let yuv = bgra_to_i420_with_matrix(&bgra, w, h, stride, ColorMatrix::Bt601);
            let mut ref_yuv = vec![0u8; w * h + 2 * (w / 2) * (h / 2)];
            let (y_part, uv_part) = ref_yuv.split_at_mut(w * h);
            let (u_part, v_part) = uv_part.split_at_mut((w / 2) * (h / 2));
            let (kyr, kyg, kyb, kur, kug, kub, kvr, kvg, kvb) = matrix_coeffs(ColorMatrix::Bt601);
            y_plane_scalar(&bgra, w, h, stride, kyr, kyg, kyb, y_part);
            uv_plane_scalar(&bgra, w, h, stride, kur, kug, kub, kvr, kvg, kvb, u_part, v_part);
            assert_eq!(yuv, ref_yuv, "extreme fill {fill} mismatch");
        }
    }

    /// 性能 sanity（手动跑：`cargo test --release -- --ignored simd_bench_1080p --nocapture`）。
    /// 同时测标量参考路径，报告加速比——只打印不断言（避免 CI 波动 flaky）。
    #[test]
    #[ignore]
    fn simd_bench_1080p() {
        let w = 1920usize;
        let h = 1080usize;
        let stride = w * 4;
        let bgra = make_bgra(w, h, stride, 0xbe);
        let n = 30;
        // SIMD dispatch 路径。
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let _ = bgra_to_i420(&bgra, w, h, stride);
        }
        let simd_ms = t0.elapsed().as_millis() as f64 / n as f64;
        // 标量参考路径（手工逐字节相同算法，不经 dispatch）。
        let (kyr, kyg, kyb, kur, kug, kub, kvr, kvg, kvb) = matrix_coeffs(ColorMatrix::Bt601);
        let y_len = w * h;
        let uv_len = y_len / 4;
        let t1 = std::time::Instant::now();
        for _ in 0..n {
            let mut yuv = vec![0u8; y_len + 2 * uv_len];
            let (y_part, uv_part) = yuv.split_at_mut(y_len);
            let (u_part, v_part) = uv_part.split_at_mut(uv_len);
            y_plane_scalar(&bgra, w, h, stride, kyr, kyg, kyb, y_part);
            uv_plane_scalar(&bgra, w, h, stride, kur, kug, kub, kvr, kvg, kvb, u_part, v_part);
        }
        let scalar_ms = t1.elapsed().as_millis() as f64 / n as f64;
        #[cfg(target_arch = "x86_64")]
        println!(
            "1080p avg: simd {simd_ms:.2} ms vs scalar {scalar_ms:.2} ms (sse4.1={}) → {:.1}x",
            std::arch::is_x86_feature_detected!("sse4.1"),
            scalar_ms / simd_ms
        );
        #[cfg(not(target_arch = "x86_64"))]
        println!("1080p avg: scalar {scalar_ms:.2} ms");
    }
}