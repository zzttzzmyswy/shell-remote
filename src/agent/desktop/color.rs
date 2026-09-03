//! BGRA → I420 (YUV 4:2:0 planar) color conversion.
//!
//! Screen captures arrive as packed 32-bit BGRA pixels (X11 ShmImage /
//! Windows GDI BitBlt both produce B, G, R, A byte order on little-endian
//! hosts). OpenH264 consumes planar I420. The conversion is scalar BT.601-ish
//! with studio-swing clamps; correctness is unit-tested directly so the rest
//! of the pipeline can trust its layout.

/// Convert a packed BGRA buffer (4 bytes/pixel: B, G, R, A) into planar I420.
///
/// Returns `w*h + w*h/2` bytes: full-resolution Y plane, then quarter-size
/// U and V planes in that order. `stride` is the source row stride in bytes
/// (typically `w*4`, but X11 images may add padding).
///
/// # Panics
///
/// Panics if `w` or `h` is odd (4:2:0 requires even dimensions), if `bgra` is
/// shorter than `h*stride`, or if the internal OOB access would read past the
/// buffer (guarded by an explicit len check).
pub fn bgra_to_i420(bgra: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
    assert!(w % 2 == 0 && h % 2 == 0, "size must be even for 4:2:0");
    assert!(bgra.len() >= h * stride, "source buffer too small");
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
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_part[row * w + col] = y.clamp(16, 235) as u8;
            if col % 2 == 0 && row % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                u_part[(row / 2) * sw + col / 2] = u.clamp(16, 240) as u8;
                v_part[(row / 2) * sw + col / 2] = v.clamp(16, 240) as u8;
            }
        }
    }
    yuv
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