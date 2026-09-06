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
    let sw = w / 2;
    for row in 0..h {
        for col in 0..w {
            let src = row * stride + col * 4;
            let b = bgra[src] as i32;
            let g = bgra[src + 1] as i32;
            let r = bgra[src + 2] as i32;
            let y = ((kyr * r + kyg * g + kyb * b + 128) >> 8) + 16;
            y_part[row * w + col] = y.clamp(16, 235) as u8;
            if col % 2 == 0 && row % 2 == 0 {
                let u = ((kur * r + kug * g + kub * b + 128) >> 8) + 128;
                let v = ((kvr * r + kvg * g + kvb * b + 128) >> 8) + 128;
                u_part[(row / 2) * sw + col / 2] = u.clamp(16, 240) as u8;
                v_part[(row / 2) * sw + col / 2] = v.clamp(16, 240) as u8;
            }
        }
    }
    yuv
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
}