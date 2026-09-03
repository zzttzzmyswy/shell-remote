//! Adaptive bitrate control (default range: 200–800 kbps per user request).
//!
//! The encoder target is adjusted from the midpoint toward the observed bit
//! usage: when the 2s sliding window shows actual bps deviating from the
//! current target by more than 20%, the target steps toward the observed
//! direction (step size proportional to deviation, clamped to [1%, 20%] of
//! the configured maximum). This gives fast recovery from scene changes while
//! staying stable under steady content.

use std::collections::VecDeque;

/// Sliding-window adaptive bitrate controller.
pub struct Abr {
    min_bps: u64,
    max_bps: u64,
    target_bps: u64,
    window: VecDeque<(f64, usize)>, // (elapsed seconds, frame bytes)
}

impl Abr {
    /// Create a controller targeting the midpoint of `[min_bps, max_bps]`.
    pub fn new(min_bps: u64, max_bps: u64) -> Self {
        assert!(min_bps <= max_bps && max_bps > 0);
        let target_bps = (min_bps + max_bps) / 2;
        Self {
            min_bps,
            max_bps,
            target_bps,
            window: VecDeque::new(),
        }
    }

    /// Current suggested bitrate in bits per second.
    pub fn target_bps(&self) -> u64 {
        self.target_bps
    }

    /// Record one encoded frame; returns the (possibly updated) target bps.
    pub fn note_frame(&mut self, now_secs: f64, encoded_bytes: usize) -> u64 {
        self.window.push_back((now_secs, encoded_bytes));
        while self
            .window
            .front()
            .map_or(false, |&(t, _)| now_secs - t > 2.0)
        {
            self.window.pop_front();
        }
        let first = self.window.front().map_or(now_secs, |&(t, _)| t);
        let last = self.window.back().map_or(now_secs, |&(t, _)| t);
        let n = self.window.len();
        if n < 2 {
            return self.target_bps;
        }
        // 时长为"末帧时刻-首帧时刻+平均帧间隔"：首帧之前的半个周期也属于
        // 这组字节的产生时间, 否则首帧密集下会高估码率。
        let interval = (last - first) / (n - 1) as f64;
        let duration = last - first + interval;
        let bytes: usize = self.window.iter().map(|&(_, b)| b).sum();
        let actual = (bytes as f64) * 8.0 / duration; // bps

        let target = self.target_bps.max(1) as f64;
        let rel = (actual - target) / target;
        if rel.abs() <= 0.2 {
            return self.target_bps;
        }
        let step = (0.02 * self.max_bps as f64 * rel.abs())
            .clamp(0.01 * self.max_bps as f64, 0.20 * self.max_bps as f64);
        let next = if rel > 0.0 {
            self.target_bps.saturating_sub(step as u64)
        } else {
            self.target_bps.saturating_add(step as u64)
        };
        self.target_bps = next.clamp(self.min_bps, self.max_bps);
        self.target_bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_target_is_midpoint() {
        let a = Abr::new(200_000, 800_000);
        assert_eq!(a.target_bps(), 500_000);
    }

    #[test]
    fn test_overload_drives_target_down_and_clamps() {
        let mut a = Abr::new(200_000, 800_000);
        // 90KB / 100ms = 7.2 Mbps, 远超上限 → 反复触发下调
        for i in 0..60 {
            a.note_frame(0.1 * i as f64 + 0.1, 90_000);
        }
        assert!(a.target_bps() < 400_000, "target={}", a.target_bps());
        // 继续无限超载 → 应 clamp 在下限
        for i in 0..200 {
            a.note_frame(0.1 * i as f64 + 20.0, 90_000);
        }
        assert_eq!(a.target_bps(), 200_000);
        assert!(a.target_bps() >= a.min_bps);
    }

    #[test]
    fn test_underload_drives_target_up_to_max() {
        let mut a = Abr::new(200_000, 800_000);
        // 2KB / 100ms = 160 kbps, 严重低于目标 → 上调
        for i in 0..180 {
            a.note_frame(0.1 * i as f64 + 0.1, 2_000);
        }
        assert!(a.target_bps() > 500_000, "target={}", a.target_bps());
        // 继续低载 → 升到上限
        for i in 0..300 {
            a.note_frame(0.1 * i as f64 + 30.0, 2_000);
        }
        assert_eq!(a.target_bps(), 800_000);
    }

    #[test]
    fn test_matching_utilization_keeps_target() {
        let mut a = Abr::new(200_000, 800_000);
        // 目标 500kbps, 10fps (0.1s), 每帧 6250 字节 = 500kbps 正好
        for i in 0..30 {
            let before = a.target_bps();
            a.note_frame(0.1 * i as f64 + 0.1, 6_250);
            let after = a.target_bps();
            assert!(
                (after as i64 - before as i64).abs() < 10_000,
                "noise-free utilization must not churn target: {before} -> {after}"
            );
        }
    }
}