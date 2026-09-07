//! Windows DXGI Desktop Duplication capture (IDXGIOutputDuplication).
//!
//! GDI `BitBlt` is a CPU copy path costing 15-40ms per 1080p frame — enough
//! to starve 60fps capture (the user's RDP machine tops out ~20fps on GDI).
//! Desktop Duplication hands us the DWM's composed desktop surface directly:
//! `AcquireNextFrame` blocks until the desktop changes (no busy polling) and
//! the readback is a single GPU→CPU staging copy. This is the capture path
//! RustDesk/Parsec/OBS all use on Windows.
//!
//! Known limits (mirrors RustDesk's dxgi module): outright fails on
//! headless RDP sessions (`DXGI_ERROR_UNSUPPORTED`) and pauses during the
//! secure desktop (UAC prompt). On failure the capture chain falls back to
//! GDI (see `capture::open_source`); `--desktop-capture gdi` forces the old
//! path.
//!
//! 多显示器（MYS-954）：按 `EnumOutputs` 顺序枚举所有桌面输出，每个输出
//! 独立 `DuplicateOutput`（一个 duplication 只能绑一个 output）。选择值 =
//! 全局 output 序号（`open(output_index)`），`list_monitors_static` 供
//! `desktop:started` 上报拓扑（名称 `DXGI<ai>.<oi>` + 桌面坐标/分辨率）。

#![cfg(windows)]

use super::capture::{Frame, FrameSource, MonitorInfo};

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// Enumerate every desktop output across all adapters (in DXGI enumeration
/// order). Returns `(adapter_index, output_index, IDXGIOutput1, desc)`.
/// 失败的 output（cast 到 IDXGIOutput1 失败）跳过。
pub(crate) fn enumerate_outputs() -> Vec<(u32, u32, IDXGIOutput1, DXGI_OUTPUT_DESC)> {
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    let mut ai: u32 = 0;
    while let Ok(a) = unsafe { factory.EnumAdapters1(ai) } {
        let mut oi: u32 = 0;
        while let Ok(o) = unsafe { a.EnumOutputs(oi) } {
            if let Ok(o1) = o.cast::<IDXGIOutput1>() {
                if let Ok(desc) = unsafe { o.GetDesc() } {
                    out.push((ai, oi, o1, desc));
                }
            }
            oi += 1;
        }
        ai += 1;
    }
    out
}

fn desc_to_monitor_name(desc: &DXGI_OUTPUT_DESC, ai: u32, oi: u32) -> String {
    let device = {
        let arr: &[u16] = &desc.DeviceName;
        let len = arr.iter().position(|&c| c == 0).unwrap_or(arr.len());
        String::from_utf16_lossy(&arr[..len])
    };
    if device.is_empty() {
        format!("DXGI{ai}.{oi}")
    } else {
        device
    }
}

/// 静态枚举远端显示器拓扑（无需 duplication）：DXGI 输出名称 + 桌面坐标
/// 分辨率。`desktop:started.displays` 上报用（MYS-954 Windows 多屏）。
pub(crate) fn list_monitors_static() -> Vec<MonitorInfo> {
    enumerate_outputs()
        .into_iter()
        .map(|(ai, oi, _o1, desc)| MonitorInfo {
            name: desc_to_monitor_name(&desc, ai, oi),
            width: (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0) as u32,
            height: (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0) as u32,
            x: desc.DesktopCoordinates.left,
            y: desc.DesktopCoordinates.top,
        })
        .collect()
}

/// One duplication session. Self-heals on access-lost (mode switch, session
/// change, secure desktop transit) by rebuilding every object once.
pub struct DxgiSource {
    width: usize,
    height: usize,
    /// 选中的全局 output 序号（enumerate_outputs 的下标）。access-lost
    /// 重建时用它找回同一个 output。
    output_index: usize,
    _factory: IDXGIFactory1,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    /// 上一帧（静止桌面 WAIT_TIMEOUT 时复用）。
    last_frame: Vec<u8>,
}

unsafe impl Send for DxgiSource {}

impl DxgiSource {
    pub fn open() -> Result<Self, String> {
        Self::open_at(0)
    }

    /// Open duplication on the `index`-th desktop output (0 = primary, order
    /// follows DXGI enumeration). 越界 = 明确报错（浏览器选屏值过期，如
    /// 拔掉显示器后未刷新拓扑——重建流即可恢复）。
    pub fn open_at(index: usize) -> Result<Self, String> {
        unsafe { Self::build(index) }
    }

    unsafe fn build(index: usize) -> Result<Self, String> {
        // 1. DXGI factory → enumerate all outputs → pick the requested one.
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let mut picked: Option<(IDXGIAdapter1, IDXGIOutput1)> = None;
        let mut cur: usize = 0;
        let mut ai: u32 = 0;
        while let Ok(a) = factory.EnumAdapters1(ai) {
            let mut oi: u32 = 0;
            while let Ok(out) = a.EnumOutputs(oi) {
                if let Ok(o1) = out.cast::<IDXGIOutput1>() {
                    if cur == index {
                        picked = Some((a.clone(), o1));
                        break;
                    }
                    cur += 1;
                }
                oi += 1;
            }
            if picked.is_some() {
                break;
            }
            ai += 1;
        }
        let (adapter, output1) =
            picked.ok_or_else(|| format!("no DXGI output #{index} (only {cur} outputs)"))?;

        // 2. D3D11 device on the duplication adapter (must match, else
        // DuplicateOutput fails with E_INVALIDARG). When creating on an
        // explicit adapter the driver type MUST be UNKNOWN (0) — passing
        // HARDWARE with a non-null padapter is E_INVALIDARG (0x80070057),
        // the classic Desktop Duplication pitfall. Hardware first, WARP for
        // machines without a usable GPU driver.
        let mut device: Option<ID3D11Device> = None;
        let hr = D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        );
        if hr.is_err() || device.is_none() {
            device = None;
            let hr2 = D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_WARP,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            );
            if hr2.is_err() || device.is_none() {
                return Err(format!("D3D11CreateDevice: {hr2:?} (unknown-driver: {hr:?})"));
            }
        }
        let device: ID3D11Device = device.ok_or("no d3d11 device")?;
        // 截图线程化（v0.33）后 D3D11 device/context 在**独立 capture 线程**
        // 使用（创建在 tokio 线程、用在新线程）。D3D11 immediate context
        // 默认绑定创建线程，跨线程调用会导致驱动崩溃（Windows agent 打开
        // 桌面 ~3s 闪退的根因）——必须启用 multithread-protected
        // （rustdesk/scrap 同款：ID3D10Multithread::SetMultithreadProtected）。
        {
            use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
            if let Ok(mt) = device.cast::<ID3D10Multithread>() {
                let _ = unsafe { mt.SetMultithreadProtected(true) };
            }
        }
        let context: ID3D11DeviceContext =
            device.GetImmediateContext().map_err(|e| format!("GetImmediateContext: {e}"))?;

        // 3. DuplicateOutput.
        let duplication: IDXGIOutputDuplication =
            output1.DuplicateOutput(&device).map_err(|e| {
                format!(
                    "DuplicateOutput: {e} (RDP session or secure desktop? \
                     GDI fallback available via --desktop-capture gdi)"
                )
            })?;

        // 4. Size comes from the duplication desc.
        let dd = duplication.GetDesc();
        let width = dd.ModeDesc.Width as usize;
        let height = dd.ModeDesc.Height as usize;
        if width < 2 || height < 2 {
            return Err(format!("duplication size {width}x{height} too small"));
        }

        // 5. Staging texture for CPU readback (BGRA, row pitch from Map).
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width as u32,
                    Height: height as u32,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: Default::default(),
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: Default::default(),
                },
                None,
                Some(&mut staging),
            )
            .map_err(|e| format!("CreateTexture2D staging: {e}"))?;
        let staging: ID3D11Texture2D = staging.ok_or("no staging texture")?;

        Ok(Self {
            width,
            height,
            output_index: index,
            _factory: factory,
            device,
            context,
            duplication,
            staging,
            last_frame: Vec::new(),
        })
    }

    /// Acquire → CopyResource → Map → packed BGRA rows. `timeout_ms` bounds
    /// the wait for a desktop change.
    ///
    /// `DXGI_ERROR_WAIT_TIMEOUT` on a static desktop is NOT an error —
    /// Desktop Duplication only presents a new frame when something changed.
    /// We replay the last frame so the encode clock keeps advancing (the
    /// encoder outputs an empty frame for unchanged content which the
    /// heartbeat-IDR path absorbs), exactly like the Wayland backend.
    unsafe fn capture_once(&mut self, timeout_ms: u32) -> Result<Vec<u8>, String> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource);
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                // Static desktop: no new frame in this window.
                return Ok(Vec::new());
            }
            Err(e) => return Err(format!("AcquireNextFrame: {e}")),
        }
        let resource = resource.ok_or("AcquireNextFrame returned no resource")?;
        let tex: ID3D11Texture2D = resource.cast().map_err(|e| format!("cast: {e}"))?;
        self.context.CopyResource(&self.staging, &tex);
        let _ = self.duplication.ReleaseFrame();

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map: {e}"))?;

        let w = self.width;
        let h = self.height;
        let src = mapped.pData as *const u8;
        let mut bgra = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            let start = row * mapped.RowPitch as usize;
            bgra.extend_from_slice(std::slice::from_raw_parts(src.add(start), w * 4));
        }
        self.context.Unmap(&self.staging, 0);
        // 全 0 像素 = 这个输出上实际没有桌面内容（虚拟显示器/无信号输出
        // 上 DuplicateOutput 成功但永不出帧, 或出黑帧）。当作无效帧。
        Ok(bgra)
    }
}

impl DxgiSource {
    /// Open duplication AND prove it actually delivers frames: after
    /// DuplicateOutput succeeds we wait once for the first frame. On virtual
    /// display adapters (GameViewer/basic display) the API can succeed but
    /// never present a frame — returning that as `Ok` would black-screen the
    /// stream, so we surface a clear error the auto chain turns into a GDI
    /// fallback.
    pub fn open_verified() -> Result<Self, String> {
        Self::open_verified_at(0)
    }

    /// [`Self::open_verified`] 的选屏版：在 `index`-th 输出上建 duplication
    /// 并验证首帧（MYS-954 Windows 多屏）。
    pub fn open_verified_at(index: usize) -> Result<Self, String> {
        let mut s = Self::open_at(index)?;
        let first = unsafe { s.capture_once(1500)? };
        if first.is_empty() {
            return Err(
                "duplication established but no frame within 1.5s (virtual display adapter / \
                 idle GPU?) — GDI fallback recommended"
                    .to_string(),
            );
        }
        s.last_frame = first;
        Ok(s)
    }
}

impl FrameSource for DxgiSource {
    fn resolution(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn next_frame(&mut self) -> Result<Frame, String> {
        unsafe {
            match self.capture_once(200) {
                // Empty vec = WAIT_TIMEOUT（静止桌面）: 复用上一帧, 编码时钟
                // 照常推进（重复帧编码输出空帧, 心跳 IDR 路径吸收）。
                Ok(bgra) if bgra.is_empty() => {
                    if self.last_frame.is_empty() {
                        // 尚未捕获到任何帧: 延长超时再试一次拿首帧。
                        let first = self.capture_once(1000)?;
                        if first.is_empty() {
                            return Err("no frame from desktop duplication yet".to_string());
                        }
                        self.last_frame = first.clone();
                        Ok(Frame {
                            bgra: first,
                            width: self.width,
                            height: self.height,
                        })
                    } else {
                        Ok(Frame {
                            bgra: self.last_frame.clone(),
                            width: self.width,
                            height: self.height,
                        })
                    }
                }
                Ok(bgra) => {
                    self.last_frame = bgra.clone();
                    Ok(Frame {
                        bgra,
                        width: self.width,
                        height: self.height,
                    })
                }
                Err(e) => {
                    // Access lost / device removed → rebuild the whole
                    // duplication once (mode switch, session reconnect).
                    if e.contains("ACCESS_LOST") || e.contains("DEVICE_REMOVED") {
                        let (w, h) = (self.width, self.height);
                        match Self::build(self.output_index) {
                            Ok(s) => {
                                *self = s;
                                tracing::info!("dxgi duplication rebuilt after access loss");
                            }
                            Err(re) => {
                                self.width = w;
                                self.height = h;
                                return Err(format!("dxgi rebuild failed: {re}"));
                            }
                        }
                        let bgra = self.capture_once(1000)?;
                        if bgra.is_empty() {
                            return Err("no frame after dxgi rebuild".to_string());
                        }
                        self.last_frame = bgra.clone();
                        return Ok(Frame {
                            bgra,
                            width: self.width,
                            height: self.height,
                        });
                    }
                    Err(e)
                }
            }
        }
    }

    fn list_monitors(&self) -> Vec<MonitorInfo> {
        list_monitors_static()
    }
}
