//! On-demand screen capture, built-in Windows OCR and JPEG encoding.
//! Nothing here runs unless the user asks for a screen read, and the pixels
//! only live in memory: no file is ever written.

use std::ffi::c_void;
use windows::{
    Graphics::Imaging::{BitmapEncoder, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream},
    Win32::{
        Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
            ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
        },
        System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED},
        UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    },
};

/// A screenshot of the whole virtual desktop (all monitors) as BGRA pixels.
pub struct Shot {
    pub width: i32,
    pub height: i32,
    pub bgra: Vec<u8>,
}

fn text(error: windows::core::Error) -> String {
    error.to_string()
}

/// Captures the screen. Call it from the UI thread after hiding the app's own windows.
pub fn capture_screen() -> Result<Shot, String> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if width <= 0 || height <= 0 {
            return Err("Could not read the screen size".into());
        }

        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err("Could not access the screen".into());
        }
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, bitmap);

        let copied = BitBlt(memory, 0, 0, width, height, screen, x, y, SRCCOPY | CAPTUREBLT);

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative = top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let rows = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut c_void),
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(memory, previous);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);

        copied.map_err(|error| format!("Screen capture failed: {error}"))?;
        if rows == 0 {
            return Err("Screen capture returned no pixels".into());
        }
        // GDI leaves the alpha byte at 0; make every pixel opaque.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Ok(Shot { width, height, bgra: pixels })
    }
}

/// Windows Runtime calls need COM on the calling thread. Harmless if repeated.
fn init_runtime() {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

fn software_bitmap(shot: &Shot) -> Result<SoftwareBitmap, String> {
    let writer = DataWriter::new().map_err(text)?;
    writer.WriteBytes(&shot.bgra).map_err(text)?;
    let buffer = writer.DetachBuffer().map_err(text)?;
    SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, shot.width, shot.height).map_err(text)
}

/// Reads the text on the screenshot with the OCR engine built into Windows.
/// Runs on a worker thread (it blocks while Windows recognises the text).
pub fn ocr(shot: &Shot) -> Result<String, String> {
    init_runtime();
    let max = OcrEngine::MaxImageDimension().map_err(text)? as i32;
    if shot.width > max || shot.height > max {
        return Err(format!(
            "The screen ({}x{}) is larger than Windows OCR supports ({max} pixels per side)",
            shot.width, shot.height
        ));
    }
    let bitmap = software_bitmap(shot)?;
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|_| "Windows OCR is not available for your languages. Add an English language pack in Windows settings.".to_string())?;
    let result = engine.RecognizeAsync(&bitmap).map_err(text)?.get().map_err(text)?;
    Ok(result.Text().map_err(text)?.to_string())
}

/// Encodes the screenshot as a JPEG for questions about colours, layout or pictures.
pub fn jpeg(shot: &Shot) -> Result<Vec<u8>, String> {
    init_runtime();
    let bitmap = software_bitmap(shot)?;
    let stream = InMemoryRandomAccessStream::new().map_err(text)?;
    let encoder = BitmapEncoder::CreateAsync(BitmapEncoder::JpegEncoderId().map_err(text)?, &stream)
        .map_err(text)?
        .get()
        .map_err(text)?;
    encoder.SetSoftwareBitmap(&bitmap).map_err(text)?;
    encoder.FlushAsync().map_err(text)?.get().map_err(text)?;

    let size = stream.Size().map_err(text)? as u32;
    let input = stream.GetInputStreamAt(0).map_err(text)?;
    let reader = DataReader::CreateDataReader(&input).map_err(text)?;
    reader.LoadAsync(size).map_err(text)?.get().map_err(text)?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes).map_err(text)?;
    Ok(bytes)
}
