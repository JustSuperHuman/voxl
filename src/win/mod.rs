//! Windows backend: the same modules as the X11 one (capture, desktop, input, audio,
//! screen) on Win32 APIs, so the rest of voxl (encoder, flow control, server, web
//! client) runs unchanged.
//!
//! - capture: DXGI Desktop Duplication, BGRA frames with dirty and move rectangles
//! - input: SendInput (scan codes, keys resolved against the current layout)
//! - desktop: cursor images from GetCursorInfo, clipboard sync (text and images)
//! - audio: system audio from WASAPI loopback
//! - screen: dynamic resolution by switching display modes
//! - mic: browser microphone played into a virtual audio cable

pub mod audio;
pub mod capture;
pub mod desktop;
pub mod input;
pub mod mic;
pub mod screen;

use parking_lot::Mutex;
use std::collections::HashMap;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};

/// Where the captured display sits on the virtual desktop, in physical pixels (voxl is
/// per-monitor DPI aware, so these are the pixels it streams).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Set by the capturer whenever it (re)starts; read by input to map coordinates.
pub static GEOMETRY: Mutex<Option<Geometry>> = Mutex::new(None);
/// Stream pixels, independent of the physical display mode.
pub static STREAM_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

struct Settings {
    /// Index into `outputs()` (0 is the primary display).
    display: usize,
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
}

static SETTINGS: Mutex<Settings> = Mutex::new(Settings {
    display: 0,
    max_width: 2560,
    max_height: 1440,
    max_pixels: 3_686_400,
});

pub fn stream_size(width: u32, height: u32) -> (u32, u32) {
    let s = SETTINGS.lock();
    fit_stream(width, height, s.max_width, s.max_height, s.max_pixels)
}

fn fit_stream(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
) -> (u32, u32) {
    let mut scale = 1.0f64;
    if max_width > 0 {
        scale = scale.min(max_width as f64 / width.max(1) as f64);
    }
    if max_height > 0 {
        scale = scale.min(max_height as f64 / height.max(1) as f64);
    }
    if max_pixels > 0 {
        scale =
            scale.min((max_pixels as f64 / (width.max(1) as f64 * height.max(1) as f64)).sqrt());
    }
    (
        ((width as f64 * scale).floor() as u32 & !1).max(2),
        ((height as f64 * scale).floor() as u32 & !1).max(2),
    )
}

/// Pixel endpoints map exactly, including monitors left of/above the primary.
pub fn stream_to_desktop(g: Geometry, stream: (u32, u32), x: i32, y: i32) -> (i32, i32) {
    let map = |p: i32, from: u32, to: u32| {
        p.clamp(0, from.saturating_sub(1) as i32) as i64 * to.saturating_sub(1) as i64
            / from.saturating_sub(1).max(1) as i64
    };
    (
        g.left + map(x, stream.0, g.width) as i32,
        g.top + map(y, stream.1, g.height) as i32,
    )
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    #[test]
    fn ultrawide_and_portrait_fit_mobile_decoder_bounds() {
        assert_eq!(fit_stream(5120, 1440, 2560, 1440, 3_686_400), (2560, 720));
        assert_eq!(fit_stream(1920, 1080, 2560, 1440, 3_686_400), (1920, 1080));
        assert_eq!(fit_stream(2160, 3840, 2560, 1440, 3_686_400), (810, 1440));
        let (w, h) = fit_stream(7680, 4320, 0, 0, 3_000_000);
        assert!(w as u64 * h as u64 <= 3_000_000 && w % 2 == 0 && h % 2 == 0);
    }
    #[test]
    fn stream_pointer_maps_all_monitor_edges_and_clamps() {
        let g = Geometry {
            left: -5120,
            top: -1440,
            width: 5120,
            height: 1440,
        };
        assert_eq!(stream_to_desktop(g, (2560, 720), 0, 0), (-5120, -1440));
        assert_eq!(stream_to_desktop(g, (2560, 720), 2559, 719), (-1, -1));
        assert_eq!(stream_to_desktop(g, (2560, 720), -100, -1), (-5120, -1440));
        assert_eq!(stream_to_desktop(g, (2560, 720), 9999, 9999), (-1, -1));
        let middle = stream_to_desktop(g, (2560, 720), 1280, 360);
        assert!((middle.0 + 2560).abs() <= 3 && (middle.1 + 720).abs() <= 3);
    }
}

/// One display attached to the desktop.
pub struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub desc: DXGI_OUTPUT_DESC,
}

impl Output {
    /// GDI device name (`\\.\DISPLAY1`), for display mode changes.
    pub fn device_name(&self) -> Vec<u16> {
        let n = &self.desc.DeviceName;
        let len = n.iter().position(|&c| c == 0).unwrap_or(n.len());
        n[..len].iter().copied().chain([0]).collect()
    }

    pub fn rect(&self) -> RECT {
        self.desc.DesktopCoordinates
    }
}

/// Displays attached to the desktop, the primary one (at the origin) first.
pub fn outputs() -> Vec<Output> {
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let mut o = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            o += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if desc.AttachedToDesktop.as_bool() {
                out.push(Output {
                    adapter: adapter.clone(),
                    output,
                    desc,
                });
            }
        }
    }
    out.sort_by_key(|o| {
        let r = o.rect();
        (r.left != 0 || r.top != 0, r.top, r.left)
    });
    out
}

/// The display voxl streams, as picked with --display.
pub fn output() -> Option<Output> {
    let i = SETTINGS.lock().display;
    outputs().into_iter().nth(i)
}

/// Reads the Windows options. Must run before anything opens a window or a device
/// context, so every coordinate is in physical pixels.
pub fn init(a: &HashMap<String, String>) {
    if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.is_err()
    {
        log::warn!("could not make voxl DPI aware; a scaled display may stream blurry");
    }
    let outs = outputs();
    for (i, o) in outs.iter().enumerate() {
        let r = o.rect();
        log::info!(
            "display {i}: {} {}x{} at ({}, {}){}",
            String::from_utf16_lossy(&o.device_name()[..o.device_name().len() - 1]),
            r.right - r.left,
            r.bottom - r.top,
            r.left,
            r.top,
            if i == 0 { " (primary)" } else { "" }
        );
    }
    let mut s = SETTINGS.lock();
    if let Some(n) = a
        .get("max-capture-width")
        .and_then(|n| n.parse::<u32>().ok())
    {
        s.max_width = n.min(16384);
    }
    if let Some(n) = a
        .get("max-capture-height")
        .and_then(|n| n.parse::<u32>().ok())
    {
        s.max_height = n.min(16384);
    }
    if let Some(n) = a
        .get("max-capture-pixels")
        .and_then(|n| n.parse::<u64>().ok())
    {
        s.max_pixels = n.min(268_435_456);
    }
    // --display: an index (0 is the primary display); ":1" style values work too.
    if let Some(n) = a
        .get("display")
        .and_then(|d| d.trim_start_matches(':').parse::<usize>().ok())
    {
        if n < outs.len() {
            s.display = n;
        } else {
            log::warn!("no display {n}; using the primary display");
        }
    }
}

/// Desktop pixels per CSS pixel to resize to: the display's scaling (1.5 at 150%).
pub fn dpi_scale() -> f64 {
    let Some(o) = output() else {
        return 1.0;
    };
    let mon: HMONITOR = o.desc.Monitor;
    let (mut x, mut y) = (96u32, 96u32);
    if unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_err() {
        return 1.0;
    }
    x as f64 / 96.0
}

/// UTF-16 with a terminating zero, for wide-string APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Text from a zero-terminated (or full) UTF-16 buffer.
pub fn from_wide(s: &[u16]) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}

/// Casts a COM interface, for the `IDXGIOutput` -> `IDXGIOutput1` kind of upgrade.
pub fn cast<T: Interface, U: Interface>(v: &T) -> Result<U, String> {
    v.cast::<U>().map_err(|e| e.to_string())
}

/// Puts this thread on the desktop that receives input (the lock screen or a UAC
/// prompt, when voxl runs as SYSTEM), so capture and input follow it. Does nothing
/// for a normal user, who can't open those desktops anyway.
pub fn follow_input_desktop() {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
        DESKTOP_CONTROL_FLAGS,
    };
    // GENERIC_ALL
    let access = DESKTOP_ACCESS_FLAGS(0x1000_0000);
    if let Ok(d) = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access) } {
        if unsafe { SetThreadDesktop(d) }.is_err() {
            let _ = unsafe { CloseDesktop(d) };
        }
    }
}
