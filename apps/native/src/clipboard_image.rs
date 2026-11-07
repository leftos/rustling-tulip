//! Reading a copied bitmap off the Windows clipboard as a PNG.
//!
//! GPUI's Windows clipboard reads PNG, GIF, JFIF, SVG, text and file lists
//! only, so a bitmap copied as `CF_DIB` or `CF_DIBV5` — what Snipping Tool and
//! many other apps leave on the clipboard — reads as nothing. This module
//! turns such a bitmap into the PNG the remote paste sends.

use anyhow::{Context, Result, bail, ensure};

/// The size of a `BITMAPINFOHEADER`.
const BITMAPINFOHEADER_SIZE: usize = 40;
/// The size of a `BITMAPV4HEADER`, the first header that carries an alpha mask.
const BITMAPV4HEADER_SIZE: usize = 108;
/// The largest width or height a copied bitmap may have.
const MAX_DIMENSION: u32 = 16_384;
/// `BI_RGB`: uncompressed pixels in the header's bit count.
const BI_RGB: u32 = 0;
/// `BI_BITFIELDS`: uncompressed pixels whose channels the colour masks name.
const BI_BITFIELDS: u32 = 3;
/// The Win32 clipboard format number of the packed DIB (`CF_DIB`).
#[cfg(windows)]
const CF_DIB: u32 = 8;
/// The Win32 clipboard format number of the V5 form (`CF_DIBV5`).
#[cfg(windows)]
const CF_DIBV5: u32 = 17;

/// Reads a copied bitmap off the system clipboard as a PNG: `None` when it
/// holds none.
pub type BitmapReader = fn() -> Result<Option<Vec<u8>>>;

/// The bitmap reader a remote image paste falls back on.
struct ClipboardBitmapReader(BitmapReader);

impl gpui::Global for ClipboardBitmapReader {}

/// Makes `reader` the bitmap reader a remote image paste falls back on when
/// GPUI's clipboard holds no image; the UI specs install one that reads
/// nothing, so they never see the developer's own clipboard.
pub fn set_bitmap_reader(cx: &mut gpui::App, reader: BitmapReader) {
    cx.set_global(ClipboardBitmapReader(reader));
}

/// The bitmap reader in use: the one [`set_bitmap_reader`] installed, else
/// [`read_clipboard_bitmap`] on Windows and none elsewhere.
pub(crate) fn bitmap_reader(cx: &gpui::App) -> BitmapReader {
    cx.try_global::<ClipboardBitmapReader>()
        .map_or(DEFAULT_READER, |reader| reader.0)
}

/// Windows' clipboard holds bitmaps GPUI does not read.
#[cfg(windows)]
const DEFAULT_READER: BitmapReader = read_clipboard_bitmap;
/// Elsewhere GPUI's clipboard reads every image there is.
#[cfg(not(windows))]
const DEFAULT_READER: BitmapReader = || Ok(None);

/// Converts a packed DIB to an RGBA PNG.
///
/// `dib` is what `GetClipboardData` returns for `CF_DIB` or `CF_DIBV5`: a
/// `BITMAPINFOHEADER`, `BITMAPV4HEADER` or `BITMAPV5HEADER`, then any colour
/// masks, any colour table, then the pixel rows. Only 24- and 32-bit pixels
/// are read, uncompressed or by colour masks.
pub(crate) fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    let header = Header::parse(dib)?;
    let rgba = header.to_rgba(dib)?;
    encode_png(header.width, header.height, &rgba)
}

/// Reads a bitmap the clipboard holds, as a PNG.
///
/// Takes `CF_DIBV5` when the clipboard has it, else `CF_DIB`, and answers
/// `None` when it holds neither.
#[cfg(windows)]
pub(crate) fn read_clipboard_bitmap() -> Result<Option<Vec<u8>>> {
    use windows::Win32::System::DataExchange::{IsClipboardFormatAvailable, OpenClipboard};

    // SAFETY: the clipboard is a process-wide resource this client opens while it reads it.
    unsafe { OpenClipboard(None) }.context("open the clipboard")?;
    let _guard = ClipboardGuard;

    let format = if unsafe { IsClipboardFormatAvailable(CF_DIBV5) }.is_ok() {
        CF_DIBV5
    } else if unsafe { IsClipboardFormatAvailable(CF_DIB) }.is_ok() {
        CF_DIB
    } else {
        return Ok(None);
    };
    let bytes = clipboard_bytes(format)?;
    dib_to_png(&bytes).map(Some)
}

/// Closes the clipboard when it goes out of scope, so every path does.
#[cfg(windows)]
struct ClipboardGuard;

#[cfg(windows)]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: the clipboard was opened by `read_clipboard_bitmap`, which holds this guard.
        unsafe {
            let _ = windows::Win32::System::DataExchange::CloseClipboard();
        }
    }
}

/// Copies the clipboard's `format` bytes out of its global memory block.
#[cfg(windows)]
fn clipboard_bytes(format: u32) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    // SAFETY: the clipboard is open, as `read_clipboard_bitmap` opened it.
    let handle = unsafe { GetClipboardData(format) }.context("read the copied bitmap")?;
    let memory = HGLOBAL(handle.0);
    // SAFETY: `memory` is a global memory handle the clipboard owns.
    let size = unsafe { GlobalSize(memory) };
    ensure!(size > 0, "clipboard bitmap: the copied bitmap is empty");
    // SAFETY: a global memory handle stays valid between its lock and unlock.
    let pointer = unsafe { GlobalLock(memory) };
    ensure!(
        !pointer.is_null(),
        "clipboard bitmap: the copied bitmap could not be locked"
    );
    let mut bytes = vec![0u8; size];
    // SAFETY: `pointer` addresses `size` readable bytes, which this copies out of it.
    unsafe { std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), bytes.as_mut_ptr(), size) };
    // SAFETY: `memory` was locked just above, so this is its matching unlock.
    let _ = unsafe { GlobalUnlock(memory) };
    Ok(bytes)
}

/// The fields of a packed DIB that decoding needs.
struct Header {
    width: u32,
    /// The row count, without the sign that marks a top-down bitmap.
    height: u32,
    top_down: bool,
    bit_count: u16,
    /// Where the pixel rows start: past the header, the masks and the colour table.
    pixel_offset: usize,
    /// One source row's length, row padding included.
    row_size: usize,
    /// Red, green, blue and alpha masks; a zero alpha mask means opaque.
    masks: [u32; 4],
}

impl Header {
    fn parse(dib: &[u8]) -> Result<Self> {
        ensure!(
            dib.len() >= BITMAPINFOHEADER_SIZE,
            "clipboard bitmap: {} bytes are shorter than the {BITMAPINFOHEADER_SIZE}-byte header",
            dib.len()
        );
        let header_size =
            usize::try_from(le_u32(dib, 0)?).context("clipboard bitmap: header size")?;
        let (width, height, top_down) = dimensions(le_i32(dib, 4)?, le_i32(dib, 8)?)?;
        let bit_count = le_u16(dib, 14)?;
        ensure!(
            bit_count == 24 || bit_count == 32,
            "clipboard bitmap: unsupported bit count {bit_count}"
        );
        let compression = le_u32(dib, 16)?;
        let masks = colour_masks(dib, header_size, compression)?;
        let pixel_offset = pixel_offset(header_size, compression, le_u32(dib, 32)?)?;
        let row_size = row_size(width, bit_count)?;
        let rows = row_size
            .checked_mul(usize::try_from(height).context("clipboard bitmap: height")?)
            .context("clipboard bitmap: the rows' length overflows")?;
        let available = dib.len().saturating_sub(pixel_offset);
        ensure!(
            available >= rows,
            "clipboard bitmap: {available} bytes of pixel data are short of the {rows} the rows need"
        );
        Ok(Self {
            width,
            height,
            top_down,
            bit_count,
            pixel_offset,
            row_size,
            masks,
        })
    }

    fn to_rgba(&self, dib: &[u8]) -> Result<Vec<u8>> {
        let width = usize::try_from(self.width).context("clipboard bitmap: width")?;
        let height = usize::try_from(self.height).context("clipboard bitmap: height")?;
        let length = self
            .row_size
            .checked_mul(height)
            .context("clipboard bitmap: the rows' length overflows")?;
        let data = dib
            .get(self.pixel_offset..self.pixel_offset.saturating_add(length))
            .context("clipboard bitmap: the pixel data is shorter than its rows")?;
        let bytes_per_pixel = if self.bit_count == 32 { 4 } else { 3 };
        let mut rgba = Vec::with_capacity(width.saturating_mul(height).saturating_mul(4));
        let lines: Vec<&[u8]> = data.chunks_exact(self.row_size).collect();
        let lines: Box<dyn Iterator<Item = &[u8]>> = if self.top_down {
            Box::new(lines.into_iter())
        } else {
            Box::new(lines.into_iter().rev())
        };
        for line in lines {
            for pixel in line.chunks_exact(bytes_per_pixel).take(width) {
                let word = pixel.iter().enumerate().fold(0u32, |word, (index, byte)| {
                    word | (u32::from(*byte) << (8 * index))
                });
                let alpha = if self.masks[3] == 0 {
                    255
                } else {
                    channel(word, self.masks[3])
                };
                rgba.extend_from_slice(&[
                    channel(word, self.masks[0]),
                    channel(word, self.masks[1]),
                    channel(word, self.masks[2]),
                    alpha,
                ]);
            }
        }
        if self.masks[3] != 0 && rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
            for pixel in rgba.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
        Ok(rgba)
    }
}

/// Reads the signed width and height, answering the row count and whether the
/// rows run top-down.
fn dimensions(width: i32, height: i32) -> Result<(u32, u32, bool)> {
    ensure!(width > 0, "clipboard bitmap: width {width} is not positive");
    ensure!(height != 0, "clipboard bitmap: height is 0");
    let width = u32::try_from(width).context("clipboard bitmap: width")?;
    let top_down = height < 0;
    let height = height.unsigned_abs();
    ensure!(
        width <= MAX_DIMENSION,
        "clipboard bitmap: width {width} exceeds {MAX_DIMENSION}"
    );
    ensure!(
        height <= MAX_DIMENSION,
        "clipboard bitmap: height {height} exceeds {MAX_DIMENSION}"
    );
    Ok((width, height, top_down))
}

/// Red, green, blue and alpha masks; a zero alpha mask means opaque.
fn colour_masks(dib: &[u8], header_size: usize, compression: u32) -> Result<[u32; 4]> {
    match compression {
        // Pixels are blue, green, red, and an unused byte at 32 bits.
        BI_RGB => Ok([0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0]),
        BI_BITFIELDS => {
            let in_header = header_size >= BITMAPV4HEADER_SIZE;
            let at = if in_header { 40 } else { header_size };
            let alpha = if in_header { le_u32(dib, 52)? } else { 0 };
            Ok([
                le_u32(dib, at)?,
                le_u32(dib, at.saturating_add(4))?,
                le_u32(dib, at.saturating_add(8))?,
                alpha,
            ])
        }
        other => bail!("clipboard bitmap: unsupported compression {other}"),
    }
}

/// Where the pixel rows start: past the header, the masks and the colour table.
fn pixel_offset(header_size: usize, compression: u32, colours_used: u32) -> Result<usize> {
    ensure!(
        header_size >= BITMAPINFOHEADER_SIZE,
        "clipboard bitmap: header size {header_size} is shorter than {BITMAPINFOHEADER_SIZE}"
    );
    let masks_follow = compression == BI_BITFIELDS && header_size < BITMAPV4HEADER_SIZE;
    let palette = usize::try_from(colours_used)
        .context("clipboard bitmap: colour table")?
        .checked_mul(4)
        .context("clipboard bitmap: colour table too large")?;
    header_size
        .checked_add(if masks_follow { 12 } else { 0 })
        .and_then(|offset| offset.checked_add(palette))
        .context("clipboard bitmap: the pixel data's offset overflows")
}

/// One source row's length in bytes, padded to a multiple of 4.
fn row_size(width: u32, bit_count: u16) -> Result<usize> {
    let bits = usize::try_from(width)
        .context("clipboard bitmap: width")?
        .checked_mul(usize::from(bit_count))
        .context("clipboard bitmap: a row's length overflows")?;
    let padded = bits
        .checked_add(31)
        .context("clipboard bitmap: a row's length overflows")?;
    Ok((padded / 32) * 4)
}

/// One 8-bit channel of `word`, read through `mask`.
fn channel(word: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let value = (word & mask) >> shift;
    let scaled = (u64::from(value) * 255 + u64::from(max) / 2) / u64::from(max);
    u8::try_from(scaled).unwrap_or(u8::MAX)
}

/// The RGBA pixels as a PNG.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    writer
        .write_image_data(rgba)
        .context("writing the PNG pixels")?;
    writer.finish().context("finishing the PNG")?;
    Ok(out)
}

/// The `len` bytes of `dib` at `at`.
fn field(dib: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    let end = at
        .checked_add(len)
        .context("clipboard bitmap: a field's offset overflows")?;
    dib.get(at..end)
        .with_context(|| format!("clipboard bitmap: missing the {len}-byte field at byte {at}"))
}

fn le_u32(dib: &[u8], at: usize) -> Result<u32> {
    let raw: [u8; 4] = field(dib, at, 4)?
        .try_into()
        .context("clipboard bitmap: a 4-byte header field")?;
    Ok(u32::from_le_bytes(raw))
}

fn le_i32(dib: &[u8], at: usize) -> Result<i32> {
    let raw: [u8; 4] = field(dib, at, 4)?
        .try_into()
        .context("clipboard bitmap: a 4-byte header field")?;
    Ok(i32::from_le_bytes(raw))
}

fn le_u16(dib: &[u8], at: usize) -> Result<u16> {
    let raw: [u8; 2] = field(dib, at, 2)?
        .try_into()
        .context("clipboard bitmap: a 2-byte header field")?;
    Ok(u16::from_le_bytes(raw))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a malformed fixture should fail the test with its message"
)]
mod tests {
    use super::dib_to_png;

    /// One packed DIB: the header, the masks a 40-byte header carries outside
    /// it, then `pixels`.
    fn dib(
        header_size: u32,
        width: i32,
        height: i32,
        bit_count: u16,
        compression: u32,
        masks: Option<[u32; 4]>,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut bytes = vec![0u8; header_size as usize];
        bytes[0..4].copy_from_slice(&header_size.to_le_bytes());
        bytes[4..8].copy_from_slice(&width.to_le_bytes());
        bytes[8..12].copy_from_slice(&height.to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&bit_count.to_le_bytes());
        bytes[16..20].copy_from_slice(&compression.to_le_bytes());
        if let Some([red, green, blue, alpha]) = masks {
            if header_size >= 108 {
                bytes[40..44].copy_from_slice(&red.to_le_bytes());
                bytes[44..48].copy_from_slice(&green.to_le_bytes());
                bytes[48..52].copy_from_slice(&blue.to_le_bytes());
                bytes[52..56].copy_from_slice(&alpha.to_le_bytes());
            } else {
                bytes.extend_from_slice(&red.to_le_bytes());
                bytes.extend_from_slice(&green.to_le_bytes());
                bytes.extend_from_slice(&blue.to_le_bytes());
            }
        }
        bytes.extend_from_slice(pixels);
        bytes
    }

    /// The PNG's width, height and RGBA pixels.
    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .expect("the PNG header");
        let size = reader.output_buffer_size().expect("a PNG buffer");
        let mut pixels = vec![0u8; size];
        let info = reader.next_frame(&mut pixels).expect("the PNG frame");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        (info.width, info.height, pixels)
    }

    #[test]
    fn a_bottom_up_24_bit_bitmap_reads_top_row_first() {
        // Bottom-up: the bottom row comes first, and each row pads to 4 bytes.
        let pixels = [
            255, 0, 0, 255, 255, 255, 0, 0, // blue and white, then 2 bytes of padding
            0, 0, 255, 0, 255, 0, 0, 0, // red and green, then 2 bytes of padding
        ];
        let png = dib_to_png(&dib(40, 2, 2, 24, 0, None, &pixels)).expect("a 24-bit bitmap");
        let (width, height, rgba) = decode(&png);
        assert_eq!((width, height), (2, 2));
        assert_eq!(
            rgba,
            vec![
                255, 0, 0, 255, // red
                0, 255, 0, 255, // green
                0, 0, 255, 255, // blue
                255, 255, 255, 255, // white
            ]
        );
    }

    #[test]
    fn a_top_down_bitmap_keeps_its_row_order() {
        // A negative height means the top row comes first.
        let pixels = [
            0, 0, 255, 0, 255, 0, 0, 0, // red and green, then 2 bytes of padding
            255, 0, 0, 255, 255, 255, 0, 0, // blue and white, then 2 bytes of padding
        ];
        let png = dib_to_png(&dib(40, 2, -2, 24, 0, None, &pixels)).expect("a top-down bitmap");
        let (width, height, rgba) = decode(&png);
        assert_eq!((width, height), (2, 2));
        assert_eq!(
            rgba,
            vec![
                255, 0, 0, 255, // red
                0, 255, 0, 255, // green
                0, 0, 255, 255, // blue
                255, 255, 255, 255, // white
            ]
        );
    }

    #[test]
    fn a_32_bit_bitmap_with_zero_alpha_is_opaque() {
        // BI_RGB 32-bit pixels are blue, green, red, then an unused byte that
        // many apps leave zero.
        let png =
            dib_to_png(&dib(40, 1, 1, 32, 0, None, &[10, 20, 30, 0])).expect("a 32-bit bitmap");
        assert_eq!(decode(&png).2, vec![30, 20, 10, 255]);
    }

    #[test]
    fn a_v5_bitmap_keeps_its_alpha_mask() {
        let masks = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000];
        // Two pixels in B, G, R, A order, the first one half transparent.
        let pixels = [0, 0, 255, 128, 0, 0, 255, 255];
        let png = dib_to_png(&dib(124, 2, 1, 32, 3, Some(masks), &pixels)).expect("a V5 bitmap");
        assert_eq!(decode(&png).2, vec![255, 0, 0, 128, 255, 0, 0, 255]);
    }

    #[test]
    fn bitfields_masks_after_a_40_byte_header_are_honoured() {
        // Red in the low byte and blue in the high one: the reverse of BI_RGB.
        let masks = [0x0000_00FF, 0x0000_FF00, 0x00FF_0000, 0];
        let png = dib_to_png(&dib(40, 1, 1, 32, 3, Some(masks), &[0x11, 0x22, 0x33, 0]))
            .expect("a bitfields bitmap");
        assert_eq!(decode(&png).2, vec![0x11, 0x22, 0x33, 255]);
    }

    #[test]
    fn row_padding_is_skipped() {
        // Three 24-bit pixels fill 9 bytes; the row pads to 12.
        let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 7, 7, 7];
        let png = dib_to_png(&dib(40, 3, 1, 24, 0, None, &pixels)).expect("a 3x1 bitmap");
        assert_eq!(
            decode(&png).2,
            vec![0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255]
        );
    }

    #[test]
    fn an_8_bit_bitmap_is_refused() {
        let error = dib_to_png(&dib(40, 1, 1, 8, 0, None, &[0])).expect_err("an 8-bit bitmap");
        assert!(
            format!("{error:#}").contains("unsupported bit count 8"),
            "{error:#}"
        );
    }

    #[test]
    fn a_truncated_bitmap_is_refused() {
        // Two 2-pixel rows of 24-bit pixels need 16 bytes; 12 are given.
        let short = dib(40, 2, 2, 24, 0, None, &[0; 12]);
        assert!(dib_to_png(&short).is_err());
    }

    #[test]
    fn a_short_header_is_refused() {
        assert!(dib_to_png(&[0u8; 20]).is_err());
    }

    #[test]
    fn a_zero_width_is_refused() {
        assert!(dib_to_png(&dib(40, 0, 1, 24, 0, None, &[])).is_err());
    }
}
