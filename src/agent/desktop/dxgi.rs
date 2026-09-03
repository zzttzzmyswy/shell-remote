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

#![cfg(windows)]

use super::capture::{Frame, FrameSource};

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// One duplication session. Self-heals on access-lost (mode switch, session
/// change, secure desktop transit) by rebuilding every object once.
pub struct DxgiSource {
    width: usize,
    height: usize,
    _factory: IDXGIFactory1,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
}

unsafe impl Send for DxgiSource {}

impl DxgiSource {
    pub fn open() -> Result<Self, String> {
        unsafe { Self::build() }
    }

    unsafe fn build() -> Result<Self, String> {
        // 1. DXGI factory → first adapter → first output → IDXGIOutput1.
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let mut adapter: Option<IDXGIAdapter1> = None;
        let mut output1: Option<IDXGIOutput1> = None;
        let mut ai: u32 = 0;
        while let Ok(a) = factory.EnumAdapters1(ai) {
            let mut oi: u32 = 0;
            while let Ok(out) = a.EnumOutputs(oi) {
                if let Ok(o1) = out.cast::<IDXGIOutput1>() {
                    adapter = Some(a.clone());
                    output1 = Some(o1);
                    break;
                }
                oi += 1;
            }
            if output1.is_some() {
                break;
            }
            ai += 1;
        }
        let adapter = adapter.ok_or_else(|| "no DXGI output found".to_string())?;
        let output1 = output1.unwrap();

        // 2. D3D11 device on the duplication adapter (must match, else
        // DuplicateOutput fails with E_INVALIDARG). Hardware first, WARP for
        // machines without a usable GPU driver.
        let mut device: Option<ID3D11Device> = None;
        let hr = D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_HARDWARE,
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
                return Err(format!("D3D11CreateDevice: {hr2:?} (hardware: {hr:?})"));
            }
        }
        let device: ID3D11Device = device.ok_or("no d3d11 device")?;
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
            _factory: factory,
            device,
            context,
            duplication,
            staging,
        })
    }

    /// Acquire → CopyResource → Map → packed BGRA rows. `timeout_ms` bounds
    /// the wait for a desktop change.
    unsafe fn capture_once(&mut self, timeout_ms: u32) -> Result<Vec<u8>, String> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        self.duplication
            .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
            .map_err(|e| format!("AcquireNextFrame: {e}"))?;
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
        Ok(bgra)
    }
}

impl FrameSource for DxgiSource {
    fn resolution(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn next_frame(&mut self) -> Result<Frame, String> {
        unsafe {
            match self.capture_once(200) {
                Ok(bgra) => Ok(Frame {
                    bgra,
                    width: self.width,
                    height: self.height,
                }),
                Err(e) => {
                    // Access lost / device removed → rebuild the whole
                    // duplication once (mode switch, session reconnect).
                    if e.contains("ACCESS_LOST") || e.contains("DEVICE_REMOVED") {
                        let (w, h) = (self.width, self.height);
                        match Self::build() {
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
}
