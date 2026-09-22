#![windows_subsystem = "windows"]
#![allow(non_snake_case, non_upper_case_globals)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Multimedia::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_NORMAL};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::*;

const MIN_W: i32 = 320;
const MAX_W: i32 = 1280;
const STEP: i32 = 80;
const WM_NEWFRAME: u32 = WM_APP + 1;
// Posted once by the capture thread as soon as it knows the real capture
// dimensions (DXGI succeeded, or GDI fell back to the virtual-desktop size),
// so the window snaps to the correct aspect ratio immediately instead of
// waiting for the user's first scroll-wheel resize.
const WM_RESIZE_TO_ASPECT: u32 = WM_APP + 2;

const MODE_DXGI: u8 = 0;
const MODE_GDI: u8 = 1;

// HWND wraps a raw pointer, so windows-rs does not mark it Send. It is just an
// opaque kernel-object handle (never dereferenced as memory by us), and the
// capture thread only ever uses it to call PostMessageW, so carrying it across
// the thread::spawn boundary this way is sound.
#[derive(Clone, Copy)]
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}

// ── Shared state between the UI thread and the capture thread ──────
struct FrameBuf {
    data: Vec<u8>,
    w: i32,
    h: i32,
}

struct SharedState {
    running: AtomicBool,
    mode: AtomicU8,
    switch_monitor: AtomicBool,
    wake_pending: AtomicBool,
    cap_x: AtomicI32,
    cap_y: AtomicI32,
    cap_w: AtomicI32,
    cap_h: AtomicI32,
    output_index: AtomicI32,
    output_count: AtomicI32,
    frame: Mutex<FrameBuf>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            mode: AtomicU8::new(MODE_DXGI),
            switch_monitor: AtomicBool::new(false),
            wake_pending: AtomicBool::new(false),
            cap_x: AtomicI32::new(0),
            cap_y: AtomicI32::new(0),
            cap_w: AtomicI32::new(0),
            cap_h: AtomicI32::new(0),
            output_index: AtomicI32::new(0),
            output_count: AtomicI32::new(1),
            frame: Mutex::new(FrameBuf { data: Vec::new(), w: 0, h: 0 }),
        }
    }
}

// Single choke point for publishing a captured frame, mirroring the C++
// PublishFrame(): resizes the shared buffer on demand (so a resolution /
// monitor change can never overflow it) and wakes the UI thread exactly
// once even if several frames land before it gets to render.
fn publish_frame(shared: &SharedState, hwnd: HWND, src: &[u8], w: i32, h: i32, src_stride: i32) {
    {
        let mut f = shared.frame.lock().unwrap();
        let needed = (w as usize) * (h as usize) * 4;
        if f.data.len() < needed {
            f.data.resize(needed, 0);
        }
        let dst_stride = (w as usize) * 4;
        if src_stride as usize == dst_stride {
            f.data[..needed].copy_from_slice(&src[..needed]);
        } else {
            for y in 0..h as usize {
                let s = y * src_stride as usize;
                let d = y * dst_stride;
                f.data[d..d + dst_stride].copy_from_slice(&src[s..s + dst_stride]);
            }
        }
        f.w = w;
        f.h = h;
    }
    if !shared.wake_pending.swap(true, Ordering::AcqRel) {
        unsafe {
            let _ = PostMessageW(hwnd, WM_NEWFRAME, WPARAM(0), LPARAM(0));
        }
    }
}

// ── DPI ──────────────────────────────────────────────────────────
fn enable_dpi_awareness() {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() {
            return;
        }
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE).is_ok() {
            return;
        }
        let _ = SetProcessDPIAware();
    }
}

// ── DXGI capture (owned entirely by the capture thread) ─────────────
struct DxgiCapture {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    adapter: IDXGIAdapter,
    output: Option<IDXGIOutput>,
    dupl: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
}

fn count_outputs(adapter: &IDXGIAdapter) -> i32 {
    let mut n = 0u32;
    unsafe {
        while n < 16 {
            match adapter.EnumOutputs(n) {
                Ok(_) => n += 1,
                Err(_) => break,
            }
        }
    }
    if n > 0 {
        n as i32
    } else {
        1
    }
}

fn create_duplication(dx: &mut DxgiCapture, shared: &SharedState) -> bool {
    dx.dupl = None;
    dx.staging = None;
    let Some(output) = dx.output.as_ref() else {
        return false;
    };
    let out1: IDXGIOutput1 = match output.cast() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let dupl = match unsafe { out1.DuplicateOutput(&dx.device) } {
        Ok(d) => d,
        Err(_) => return false,
    };

    // Trust the duplication's own reported size, not a value cached earlier —
    // this is what keeps captures correct across resolution changes / monitor
    // swaps.
    let dd = unsafe { dupl.GetDesc() };
    let cap_w = dd.ModeDesc.Width as i32;
    let cap_h = dd.ModeDesc.Height as i32;
    shared.cap_w.store(cap_w, Ordering::Relaxed);
    shared.cap_h.store(cap_h, Ordering::Relaxed);

    // Keep the selected output's virtual-desktop origin. Desktop Duplication
    // returns pixels local to one output, whereas GDI and cursor coordinates
    // are expressed against the whole virtual desktop.
    if let Ok(desc) = unsafe { output.GetDesc() } {
        shared.cap_x.store(desc.DesktopCoordinates.left, Ordering::Relaxed);
        shared.cap_y.store(desc.DesktopCoordinates.top, Ordering::Relaxed);
    }
    if cap_w <= 0 || cap_h <= 0 {
        return false;
    }

    let td = D3D11_TEXTURE2D_DESC {
        Width: cap_w as u32,
        Height: cap_h as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    if unsafe { dx.device.CreateTexture2D(&td, None, Some(&mut staging)) }.is_err() {
        return false;
    }
    dx.staging = staging;
    dx.dupl = Some(dupl);
    true
}

fn switch_to_output(dx: &mut DxgiCapture, shared: &SharedState, idx: i32) -> bool {
    dx.dupl = None;
    dx.staging = None;
    dx.output = None;
    let output = match unsafe { dx.adapter.EnumOutputs(idx as u32) } {
        Ok(o) => o,
        Err(_) => return false,
    };
    if let Ok(desc) = unsafe { output.GetDesc() } {
        let c = desc.DesktopCoordinates;
        shared.cap_x.store(c.left, Ordering::Relaxed);
        shared.cap_y.store(c.top, Ordering::Relaxed);
        shared.cap_w.store(c.right - c.left, Ordering::Relaxed);
        shared.cap_h.store(c.bottom - c.top, Ordering::Relaxed);
    }
    dx.output = Some(output);
    shared.output_index.store(idx, Ordering::Relaxed);
    create_duplication(dx, shared)
}

fn init_dxgi(shared: &SharedState) -> Option<DxgiCapture> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        if D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            Default::default(),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .is_err()
        {
            return None;
        }
        let device = device?;
        let ctx = ctx?;

        let dxgi_dev: IDXGIDevice = device.cast().ok()?;
        let adapter: IDXGIAdapter = dxgi_dev.GetParent().ok()?;

        let output_count = count_outputs(&adapter);
        shared.output_count.store(output_count, Ordering::Relaxed);
        shared.output_index.store(0, Ordering::Relaxed);
        let output = adapter.EnumOutputs(0).ok()?;

        let mut dx = DxgiCapture {
            device,
            ctx,
            adapter,
            output: Some(output),
            dupl: None,
            staging: None,
        };
        if create_duplication(&mut dx, shared) {
            Some(dx)
        } else {
            None
        }
    }
}

// ── GDI fallback capture (owned entirely by the capture thread) ────
struct GdiCapture {
    screen_dc: HDC,
    cap_dc: HDC,
    cap_bmp: Option<HBITMAP>,
    cap_w: i32,
    cap_h: i32,
    frame_delay_ms: u32,
}

impl GdiCapture {
    fn new() -> Self {
        unsafe {
            let screen_dc = GetDC(None);
            let cap_dc = CreateCompatibleDC(screen_dc);
            let refresh = GetDeviceCaps(screen_dc, VREFRESH);
            let frame_delay_ms = if refresh > 1 { 1000 / refresh as u32 } else { 16 };
            Self { screen_dc, cap_dc, cap_bmp: None, cap_w: 0, cap_h: 0, frame_delay_ms }
        }
    }

    fn ensure_sized(&mut self, w: i32, h: i32) {
        if self.cap_bmp.is_some() && self.cap_w == w && self.cap_h == h {
            return;
        }
        unsafe {
            if let Some(bmp) = self.cap_bmp.take() {
                let _ = DeleteObject(bmp.into());
            }
            let bmp = CreateCompatibleBitmap(self.screen_dc, w, h);
            let _ = SelectObject(self.cap_dc, bmp.into());
            self.cap_bmp = Some(bmp);
            self.cap_w = w;
            self.cap_h = h;
        }
    }

    fn capture_frame(&mut self, shared: &SharedState, hwnd: HWND, scratch: &mut Vec<u8>, cap_x: i32, cap_y: i32, w: i32, h: i32) {
        self.ensure_sized(w, h);
        unsafe {
            // BitBlt — works for most OpenGL/SDL exclusive fullscreen games.
            // Uses the selected monitor's virtual-desktop position instead of
            // always capturing the primary monitor at (0,0).
            let _ = BitBlt(self.cap_dc, 0, 0, w, h, self.screen_dc, cap_x, cap_y, SRCCOPY);

            let needed = (w as usize) * (h as usize) * 4;
            if scratch.len() < needed {
                scratch.resize(needed, 0);
            }
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0 as u32;
            if let Some(bmp) = self.cap_bmp {
                GetDIBits(self.cap_dc, bmp, 0, h as u32, Some(scratch.as_mut_ptr().cast()), &mut bi, DIB_RGB_COLORS);
            }
            publish_frame(shared, hwnd, scratch, w, h, w * 4);
        }
    }
}

impl Drop for GdiCapture {
    fn drop(&mut self) {
        unsafe {
            if let Some(bmp) = self.cap_bmp.take() {
                let _ = DeleteObject(bmp.into());
            }
            let _ = DeleteDC(self.cap_dc);
            ReleaseDC(None, self.screen_dc);
        }
    }
}

// ── Capture thread ───────────────────────────────────────────────
fn capture_thread(shared: Arc<SharedState>, hwnd: HWND) {
    unsafe {
        // Capture must not outrank the game or other foreground software.
        // Normal priority leaves scheduling fair while preserving an
        // uncapped capture loop.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL);
    }

    let mut dxgi = init_dxgi(&shared);
    if dxgi.is_none() {
        shared.mode.store(MODE_GDI, Ordering::Relaxed);
    }
    if shared.cap_w.load(Ordering::Relaxed) > 0 && shared.cap_h.load(Ordering::Relaxed) > 0 {
        unsafe {
            let _ = PostMessageW(hwnd, WM_RESIZE_TO_ASPECT, WPARAM(0), LPARAM(0));
        }
    }
    let mut gdi = GdiCapture::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut gdi_fail_count: u32 = 0;

    while shared.running.load(Ordering::Relaxed) {
        let mode = shared.mode.load(Ordering::Relaxed);

        if mode == MODE_DXGI {
            let dx = match dxgi.as_mut() {
                Some(dx) => dx,
                None => {
                    thread::sleep(Duration::from_millis(200));
                    if let Some(new_dx) = init_dxgi(&shared) {
                        dxgi = Some(new_dx);
                    } else {
                        shared.mode.store(MODE_GDI, Ordering::Relaxed);
                    }
                    continue;
                }
            };

            let output_count = shared.output_count.load(Ordering::Relaxed);
            if shared.switch_monitor.swap(false, Ordering::AcqRel) && output_count > 1 {
                let next = (shared.output_index.load(Ordering::Relaxed) + 1) % output_count;
                if !switch_to_output(dx, &shared, next) {
                    shared.mode.store(MODE_GDI, Ordering::Relaxed);
                }
                continue;
            }

            if dx.dupl.is_none() {
                thread::sleep(Duration::from_millis(200));
                if !create_duplication(dx, &shared) {
                    shared.mode.store(MODE_GDI, Ordering::Relaxed);
                }
                continue;
            }

            let dupl = dx.dupl.as_ref().unwrap().clone();
            let mut fi = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            let hr = unsafe { dupl.AcquireNextFrame(100, &mut fi, &mut res) };

            match hr {
                Ok(()) => {
                    if fi.LastPresentTime != 0 {
                        if let Some(res) = &res {
                            if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                                if let Some(staging) = dx.staging.as_ref() {
                                    unsafe {
                                        dx.ctx.CopyResource(staging, &tex);
                                        let mut mp = D3D11_MAPPED_SUBRESOURCE::default();
                                        if dx.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mp)).is_ok() {
                                            let cap_w = shared.cap_w.load(Ordering::Relaxed);
                                            let cap_h = shared.cap_h.load(Ordering::Relaxed);
                                            let len = (mp.RowPitch as usize) * (cap_h as usize);
                                            let slice = std::slice::from_raw_parts(mp.pData.cast::<u8>(), len);
                                            publish_frame(&shared, hwnd, slice, cap_w, cap_h, mp.RowPitch as i32);
                                            dx.ctx.Unmap(staging, 0);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    unsafe { let _ = dupl.ReleaseFrame(); }
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                    dx.dupl = None;
                    thread::sleep(Duration::from_millis(100));
                    if !create_duplication(dx, &shared) {
                        shared.mode.store(MODE_GDI, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(50));
                }
            }
        } else {
            // ── GDI fallback mode ──────────────────────────────
            // Preserve the documented M-key monitor switching when DXGI is
            // temporarily unavailable. If duplication succeeds, resume DXGI;
            // otherwise keep capturing the newly selected monitor with GDI.
            let output_count = shared.output_count.load(Ordering::Relaxed);
            if shared.switch_monitor.load(Ordering::Relaxed) && output_count > 1 {
                shared.switch_monitor.store(false, Ordering::Relaxed);
                if let Some(dx) = dxgi.as_mut() {
                    let next = (shared.output_index.load(Ordering::Relaxed) + 1) % output_count;
                    if switch_to_output(dx, &shared, next) {
                        shared.mode.store(MODE_DXGI, Ordering::Relaxed);
                        continue;
                    }
                }
            }
            shared.switch_monitor.store(false, Ordering::Relaxed);

            if shared.cap_w.load(Ordering::Relaxed) <= 0 || shared.cap_h.load(Ordering::Relaxed) <= 0 {
                // DXGI never even told us a resolution (e.g. no GPU at all) —
                // use the virtual-desktop metrics so GDI still captures the
                // whole usable desktop.
                unsafe {
                    shared.cap_x.store(GetSystemMetrics(SM_XVIRTUALSCREEN), Ordering::Relaxed);
                    shared.cap_y.store(GetSystemMetrics(SM_YVIRTUALSCREEN), Ordering::Relaxed);
                    shared.cap_w.store(GetSystemMetrics(SM_CXVIRTUALSCREEN), Ordering::Relaxed);
                    shared.cap_h.store(GetSystemMetrics(SM_CYVIRTUALSCREEN), Ordering::Relaxed);
                }
            }
            let cap_x = shared.cap_x.load(Ordering::Relaxed);
            let cap_y = shared.cap_y.load(Ordering::Relaxed);
            let cap_w = shared.cap_w.load(Ordering::Relaxed);
            let cap_h = shared.cap_h.load(Ordering::Relaxed);
            gdi.capture_frame(&shared, hwnd, &mut scratch, cap_x, cap_y, cap_w, cap_h);
            thread::sleep(Duration::from_millis(gdi.frame_delay_ms as u64));

            // Try switching back to DXGI periodically.
            gdi_fail_count += 1;
            if gdi_fail_count > 180 {
                gdi_fail_count = 0;
                if let Some(dx) = dxgi.as_mut() {
                    if dx.output.is_some() && create_duplication(dx, &shared) {
                        shared.mode.store(MODE_DXGI, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

// ── Render-side state (owned entirely by the UI thread) ─────────────
struct RenderState {
    win_w: i32,
    win_h: i32,
    dragging: bool,
    drag_off: (i32, i32),
    btn_hover: bool,
    win_dc: HDC,
    mem_dc: HDC,
    mem_bmp: Option<HBITMAP>,
    mem_w: i32,
    mem_h: i32,
    local_buf: Vec<u8>,
    fps: i32,
    f_count: i32,
    f_time: u32,
    close_bg: HBRUSH,
    close_hover_bg: HBRUSH,
    close_pen: HPEN,
    close_font: HFONT,
    overlay_font: HFONT,
    cached_cursor: HCURSOR,
    cached_cursor_w: i32,
    cached_cursor_h: i32,
    cached_cursor_hotspot: (i32, i32),
}

impl RenderState {
    fn new(hwnd: HWND) -> Self {
        unsafe {
            let win_dc = GetDC(hwnd);
            Self {
                win_w: 800,
                win_h: 450,
                dragging: false,
                drag_off: (0, 0),
                btn_hover: false,
                win_dc,
                mem_dc: HDC::default(),
                mem_bmp: None,
                mem_w: 0,
                mem_h: 0,
                local_buf: Vec::new(),
                fps: 0,
                f_count: 0,
                f_time: GetTickCount(),
                close_bg: CreateSolidBrush(COLORREF(rgb(60, 10, 10))),
                close_hover_bg: CreateSolidBrush(COLORREF(rgb(220, 40, 40))),
                close_pen: CreatePen(PS_SOLID, 1, COLORREF(rgb(200, 60, 60))),
                close_font: make_font(13, FW_BOLD.0 as i32, s!("Arial"), DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32),
                overlay_font: make_font(13, FW_NORMAL.0 as i32, s!("Consolas"), FIXED_PITCH.0 as u32 | FF_MODERN.0 as u32),
                cached_cursor: HCURSOR::default(),
                cached_cursor_w: 32,
                cached_cursor_h: 32,
                cached_cursor_hotspot: (0, 0),
            }
        }
    }

    fn ensure_mem(&mut self, w: i32, h: i32) {
        if self.mem_w == w && self.mem_h == h {
            return;
        }
        unsafe {
            if let Some(bmp) = self.mem_bmp.take() {
                let _ = DeleteObject(bmp.into());
            }
            if !self.mem_dc.is_invalid() {
                let _ = DeleteDC(self.mem_dc);
            }
            self.mem_dc = CreateCompatibleDC(self.win_dc);
            let bmp = CreateCompatibleBitmap(self.win_dc, w, h);
            let _ = SelectObject(self.mem_dc, bmp.into());
            self.mem_bmp = Some(bmp);
            self.mem_w = w;
            self.mem_h = h;
        }
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        unsafe {
            if let Some(bmp) = self.mem_bmp.take() {
                let _ = DeleteObject(bmp.into());
            }
            if !self.mem_dc.is_invalid() {
                let _ = DeleteDC(self.mem_dc);
            }
            let _ = DeleteObject(self.close_bg.into());
            let _ = DeleteObject(self.close_hover_bg.into());
            let _ = DeleteObject(self.close_pen.into());
            let _ = DeleteObject(self.close_font.into());
            let _ = DeleteObject(self.overlay_font.into());
        }
    }
}

fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

fn make_font(height: i32, weight: i32, face: PCSTR, pitch_family: u32) -> HFONT {
    unsafe {
        CreateFontA(
            height, 0, 0, 0, weight, 0, 0, 0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            pitch_family,
            face,
        )
    }
}

fn compute_win_h(w: i32, cap_w: i32, cap_h: i32) -> i32 {
    if cap_w > 0 && cap_h > 0 {
        ((w as i64) * (cap_h as i64) / (cap_w as i64)) as i32
    } else {
        w * 9 / 16
    }
}

fn is_close(w: i32, x: i32, y: i32) -> bool {
    x >= w - 30 && x < w - 2 && y >= 2 && y < 22
}

fn draw_close_btn(dc: HDC, r: &RenderState, hover: bool) {
    unsafe {
        let mut rect = RECT { left: r.win_w - 30, top: 2, right: r.win_w - 2, bottom: 22 };
        FillRect(dc, &rect, if hover { r.close_hover_bg } else { r.close_bg });
        let old_pen = SelectObject(dc, r.close_pen.into());
        let old_brush = SelectObject(dc, HGDIOBJ(GetStockObject(NULL_BRUSH).0));
        let _ = Rectangle(dc, rect.left, rect.top, rect.right, rect.bottom);
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        let old_font = SelectObject(dc, r.close_font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(rgb(255, 255, 255)));
        let mut label = *b"X";
        DrawTextA(dc, &mut label, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        SelectObject(dc, old_font);
    }
}

fn draw_cursor_overlay(dc_mem: HDC, r: &mut RenderState, shared: &SharedState) {
    unsafe {
        let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut ci).is_err() || ci.flags != CURSOR_SHOWING {
            return;
        }

        // Cursor images normally keep the same handle for many frames. Query
        // its bitmap and hotspot only when that handle changes, rather than
        // allocating temporary GDI bitmaps on every captured frame.
        if ci.hCursor != r.cached_cursor {
            let mut ii = ICONINFO::default();
            if GetIconInfo(ci.hCursor, &mut ii).is_err() {
                return;
            }
            r.cached_cursor = ci.hCursor;
            r.cached_cursor_hotspot = (ii.xHotspot as i32, ii.yHotspot as i32);
            r.cached_cursor_w = 32;
            r.cached_cursor_h = 32;
            let src_bmp = if !ii.hbmColor.is_invalid() { Some(ii.hbmColor) } else { Some(ii.hbmMask) };
            if let Some(bmp) = src_bmp {
                let mut bm = BITMAP::default();
                if GetObjectA(bmp.into(), std::mem::size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast())) != 0 {
                    r.cached_cursor_w = bm.bmWidth;
                    r.cached_cursor_h = if !ii.hbmColor.is_invalid() { bm.bmHeight } else { bm.bmHeight / 2 };
                }
            }
            if !ii.hbmMask.is_invalid() { let _ = DeleteObject(ii.hbmMask.into()); }
            if !ii.hbmColor.is_invalid() { let _ = DeleteObject(ii.hbmColor.into()); }
        }

        let cap_w = shared.cap_w.load(Ordering::Relaxed);
        let cap_h = shared.cap_h.load(Ordering::Relaxed);
        if cap_w > 0 && cap_h > 0 {
            let cap_x = shared.cap_x.load(Ordering::Relaxed);
            let cap_y = shared.cap_y.load(Ordering::Relaxed);
            let sx = r.win_w as f32 / cap_w as f32;
            let sy = r.win_h as f32 / cap_h as f32;
            let cx = ((ci.ptScreenPos.x - cap_x) as f32 * sx - r.cached_cursor_hotspot.0 as f32 * sx) as i32;
            let cy = ((ci.ptScreenPos.y - cap_y) as f32 * sy - r.cached_cursor_hotspot.1 as f32 * sy) as i32;
            let _ = DrawIconEx(
                dc_mem, cx, cy, r.cached_cursor,
                (r.cached_cursor_w as f32 * sx) as i32,
                (r.cached_cursor_h as f32 * sy) as i32,
                0, None, DI_NORMAL,
            );
        }
    }
}

fn render_frame(r: &mut RenderState, shared: &SharedState) {
    let (has, fw, fh) = {
        let f = shared.frame.lock().unwrap();
        if f.w > 0 && f.h > 0 {
            let needed = (f.w as usize) * (f.h as usize) * 4;
            if r.local_buf.len() < needed {
                r.local_buf.resize(needed, 0);
            }
            r.local_buf[..needed].copy_from_slice(&f.data[..needed]);
            (true, f.w, f.h)
        } else {
            (false, 0, 0)
        }
    };

    r.ensure_mem(r.win_w, r.win_h);
    let mem_dc = r.mem_dc;

    unsafe {
        if has && fw > 0 && fh > 0 {
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = fw;
            bi.bmiHeader.biHeight = -fh;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0 as u32;
            // This is the lowest-overhead scaling path. It keeps FixScreen
            // uncapped without the costly per-frame filtering that previously
            // reduced FPS.
            SetStretchBltMode(mem_dc, COLORONCOLOR);
            StretchDIBits(
                mem_dc, 0, 0, r.win_w, r.win_h,
                0, 0, fw, fh,
                Some(r.local_buf.as_ptr().cast()),
                &bi, DIB_RGB_COLORS, SRCCOPY,
            );
        }

        // Overlay
        let mode_str = if shared.mode.load(Ordering::Relaxed) == MODE_DXGI { "DXGI" } else { "GDI" };
        let output_index = shared.output_index.load(Ordering::Relaxed);
        let output_count = shared.output_count.load(Ordering::Relaxed);
        let txt = format!(
            "{} fps  [{}]  mon {}/{}  wheel=resize  M=monitor  RMB/X=exit\0",
            r.fps, mode_str, output_index + 1, output_count
        );
        let mut rc = RECT { left: 4, top: r.win_h - 18, right: r.win_w - 34, bottom: r.win_h };
        let old_font = SelectObject(mem_dc, r.overlay_font.into());
        SetBkMode(mem_dc, TRANSPARENT);
        let color = if shared.mode.load(Ordering::Relaxed) == MODE_DXGI { rgb(0, 220, 80) } else { rgb(255, 180, 0) };
        SetTextColor(mem_dc, COLORREF(color));
        DrawTextA(mem_dc, &mut txt.into_bytes(), &mut rc, DT_LEFT | DT_TOP | DT_SINGLELINE);
        SelectObject(mem_dc, old_font);

        draw_close_btn(mem_dc, r, r.btn_hover);
        draw_cursor_overlay(mem_dc, r, shared);

        let _ = BitBlt(r.win_dc, 0, 0, r.win_w, r.win_h, mem_dc, 0, 0, SRCCOPY);
    }

    r.f_count += 1;
    let now = unsafe { GetTickCount() };
    if now.wrapping_sub(r.f_time) >= 1000 {
        r.fps = r.f_count;
        r.f_count = 0;
        r.f_time = now;
    }
}

// ── Window procedure ────────────────────────────────────────────────
struct AppState {
    shared: Arc<SharedState>,
    render: RefCell<RenderState>,
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTA);
        let shared_ptr = cs.lpCreateParams as *const SharedState;
        if !shared_ptr.is_null() {
            // Reclaim the Arc clone that main() sent through lpCreateParams.
            // `hwnd` here is already the real, valid window handle (Windows
            // creates the HWND before sending WM_NCCREATE), so RenderState
            // can be built once, correctly, right here.
            let shared = Arc::from_raw(shared_ptr);
            let app_state = Box::new(AppState {
                shared,
                render: RefCell::new(RenderState::new(hwnd)),
            });
            let raw = Box::into_raw(app_state);
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        }
        return DefWindowProcA(hwnd, msg, wparam, lparam);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AppState;
    if ptr.is_null() {
        return DefWindowProcA(hwnd, msg, wparam, lparam);
    }
    let app = &*ptr;

    match msg {
        WM_NCDESTROY => {
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr as *mut AppState));
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                let _ = DestroyWindow(hwnd);
            } else if wparam.0 as u8 as char == 'M' {
                app.shared.switch_monitor.store(true, Ordering::Relaxed);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let mx = (lparam.0 & 0xFFFF) as i16 as i32;
            let my = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut r = app.render.borrow_mut();
            if is_close(r.win_w, mx, my) {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            r.dragging = true;
            let _ = SetCapture(hwnd);
            let mut c = POINT::default();
            let _ = GetCursorPos(&mut c);
            let mut w = RECT::default();
            let _ = GetWindowRect(hwnd, &mut w);
            r.drag_off = (c.x - w.left, c.y - w.top);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            app.render.borrow_mut().dragging = false;
            let _ = ReleaseCapture();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mx = (lparam.0 & 0xFFFF) as i16 as i32;
            let my = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut r = app.render.borrow_mut();
            r.btn_hover = is_close(r.win_w, mx, my);
            if r.dragging {
                let mut c = POINT::default();
                let _ = GetCursorPos(&mut c);
                let _ = SetWindowPos(
                    hwnd, HWND_TOPMOST,
                    c.x - r.drag_off.0, c.y - r.drag_off.1, 0, 0,
                    SWP_NOSIZE,
                );
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 as u32 >> 16) as i16) as i32;
            let d = if delta > 0 { 1 } else { -1 };
            let mut r = app.render.borrow_mut();
            let nw = (r.win_w + d * STEP).clamp(MIN_W, MAX_W);
            r.win_w = nw;
            r.win_h = compute_win_h(nw, app.shared.cap_w.load(Ordering::Relaxed), app.shared.cap_h.load(Ordering::Relaxed));
            let (w, h) = (r.win_w, r.win_h);
            drop(r);
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, w, h, SWP_NOMOVE);
            LRESULT(0)
        }
        WM_NEWFRAME => {
            app.shared.wake_pending.store(false, Ordering::Release);
            render_frame(&mut app.render.borrow_mut(), &app.shared);
            LRESULT(0)
        }
        WM_RESIZE_TO_ASPECT => {
            let mut r = app.render.borrow_mut();
            let w = r.win_w;
            r.win_h = compute_win_h(w, app.shared.cap_w.load(Ordering::Relaxed), app.shared.cap_h.load(Ordering::Relaxed));
            let h = r.win_h;
            drop(r);
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, w, h, SWP_NOMOVE);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let r = app.render.borrow();
            if !r.mem_dc.is_invalid() {
                let _ = BitBlt(hdc, 0, 0, r.win_w, r.win_h, r.mem_dc, 0, 0, SRCCOPY);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcA(hwnd, msg, wparam, lparam),
    }
}

fn main() -> Result<()> {
    unsafe {
        enable_dpi_awareness(); // must happen before any GetSystemMetrics / window creation
        let _ = timeBeginPeriod(1);

        let shared = Arc::new(SharedState::new());
        let hinstance: HINSTANCE = GetModuleHandleA(None)?.into();

        let class_name = s!("MiniDesktopRust");
        let wc = WNDCLASSEXA {
            cbSize: std::mem::size_of::<WNDCLASSEXA>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        let _ = RegisterClassExA(&wc);

        let win_w = 800;
        let win_h = 450;
        let sx = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
        let sy = (GetSystemMetrics(SM_CYSCREEN) - win_h) / 2;

        // One Arc clone is sent through lpCreateParams as a raw pointer; the
        // WM_NCCREATE handler above reclaims it (Arc::from_raw) and builds
        // AppState using the real hwnd, which already exists at that point.
        let shared_for_window = Arc::into_raw(Arc::clone(&shared)) as *const c_void;

        let hwnd = CreateWindowExA(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name,
            s!("Mini Desktop"),
            WS_POPUP | WS_VISIBLE,
            sx, sy, win_w, win_h,
            None, None, hinstance,
            Some(shared_for_window),
        )?;
        if hwnd.is_invalid() {
            return Err(Error::from_win32());
        }

        let hwnd_send = SendHwnd(hwnd);
        let hT = thread::Builder::new()
            .spawn({
                let shared = Arc::clone(&shared);
                move || capture_thread(shared, hwnd_send.0)
            })
            .expect("failed to start capture thread");

        let mut msg = MSG::default();
        loop {
            let r = GetMessageA(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }

        shared.running.store(false, Ordering::Relaxed);
        let _ = hT.join();
        timeEndPeriod(1);
        Ok(())
    }
}

