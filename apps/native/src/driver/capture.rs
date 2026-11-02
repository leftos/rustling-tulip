//! The `screenshot` capture: the main window's client area through
//! `PrintWindow`, written as a PNG. It runs on a worker thread, never on the
//! UI thread, since `PrintWindow` sends the window a message that the UI
//! thread answers.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWM_CLOAKED_APP, DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible,
    PW_RENDERFULLCONTENT,
};

/// A capture of the client area: top-down rows of BGRA pixels.
struct Frame {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

/// Captures the window whose handle is `address` and writes it to `out` as
/// a PNG, returning its size in physical pixels.
///
/// # Errors
///
/// Why the capture was refused or failed: the window is not cloaked, is the
/// foreground window, belongs to another process, captured black, or a
/// call failed.
pub fn capture_png(address: usize, out: &Path) -> Result<(u32, u32)> {
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    check_out_of_the_way(hwnd)?;
    let frame = capture(hwnd)?;
    let blank = frame
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .all(|&[blue, green, red, _]| [blue, green, red] == [0, 0, 0]);
    if blank {
        bail!("the window captured black: it drew nothing");
    }
    write_png(&frame, out)?;
    Ok((frame.width, frame.height))
}

/// Whether the window whose handle is `address` is shown and cloaked by
/// the app: a window not yet shown, or cloaked only by the shell, is not.
///
/// # Errors
///
/// When its cloaked state cannot be read.
pub fn is_out_of_sight(address: usize) -> Result<bool> {
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    Ok(shown(hwnd) && cloaked_by_app(hwnd)?)
}

fn shown(hwnd: HWND) -> bool {
    // SAFETY: a plain query; a stale handle answers false.
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}

fn cloaked_by_app(hwnd: HWND) -> Result<bool> {
    let mut cloaked: u32 = 0;
    let size = u32::try_from(size_of::<u32>())?;
    // SAFETY: an out-pointer to a local of the size passed, on a window
    // handle; a stale handle fails the call.
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size) }
        .context("reading the window's cloaked state")?;
    Ok(cloaked & DWM_CLOAKED_APP != 0)
}

/// The client area through `PrintWindow` with `PW_RENDERFULLCONTENT`,
/// which reads the compositor's copy of a cloaked window without showing,
/// moving or activating it.
fn capture(hwnd: HWND) -> Result<Frame> {
    let mut rect = RECT::default();
    // SAFETY: an out-pointer to a local, on a live window handle.
    unsafe { GetClientRect(hwnd, &raw mut rect) }.context("reading the client rect")?;
    let (width, height) = (rect.right, rect.bottom);
    if width <= 0 || height <= 0 {
        bail!("the window has no client area ({width}x{height})");
    }
    let pixels = usize::try_from(width)? * usize::try_from(height)?;
    let mut bgra = vec![0u8; pixels * 4];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>())?,
            biWidth: width,
            // Negative: top-down rows.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT);
    let rows = u32::try_from(height)?;
    // SAFETY: the DCs and bitmap are created, used and released here, on a
    // live window; `bgra` holds the bitmap's full size in the format `info`
    // describes.
    let (printed, lines) = unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(dc, bitmap.into());
        let printed = PrintWindow(hwnd, dc, flags);
        SelectObject(dc, previous);
        let bits = Some(bgra.as_mut_ptr().cast());
        let lines = GetDIBits(dc, bitmap, 0, rows, bits, &raw mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        (printed.as_bool(), lines)
    };
    if !printed {
        bail!("PrintWindow failed on the driven window");
    }
    if lines != height {
        bail!("GetDIBits copied {lines} of {height} rows");
    }
    check_out_of_the_way(hwnd)?;
    Ok(Frame {
        width: u32::try_from(width)?,
        height: rows,
        bgra,
    })
}

/// Fails unless the window is cloaked, is not the foreground window and
/// belongs to this process: a capture must never read a window the user
/// sees or one that is not the client's own.
fn check_out_of_the_way(hwnd: HWND) -> Result<()> {
    if !shown(hwnd) {
        bail!("the driven window is not shown yet");
    }
    if !cloaked_by_app(hwnd)? {
        bail!("the driven window is not cloaked by the client, so it may show on screen");
    }
    // SAFETY: a plain query.
    if unsafe { GetForegroundWindow() } == hwnd {
        bail!("the driven window is the foreground window");
    }
    let mut owner: u32 = 0;
    // SAFETY: an out-pointer to a local; a stale handle answers thread 0.
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut owner)) };
    if thread == 0 || owner != std::process::id() {
        bail!("the window belongs to another process ({owner})");
    }
    Ok(())
}

/// Writes `frame` to `out` as an RGBA PNG; the capture's alpha is
/// undefined, so every pixel is written opaque.
fn write_png(frame: &Frame, out: &Path) -> Result<()> {
    let mut rgba = Vec::with_capacity(frame.bgra.len());
    for &[blue, green, red, _] in frame.bgra.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[red, green, blue, u8::MAX]);
    }
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    writer
        .write_image_data(&rgba)
        .context("writing the PNG pixels")?;
    writer.finish().context("finishing the PNG")
}
