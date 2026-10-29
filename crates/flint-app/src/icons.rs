//! Real Windows icons for result rows, as PNG bytes for the
//! WebView. One shell call per extension; files that carry their own icon
//! (.exe, .lnk, .ico, .url) per path; folders once.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Types whose files carry their own icon. Not ".url": an internet
/// shortcut's icon may live on another computer, and asking for it would
/// contact that computer (with the user's sign-in) for any such file found.
const OWN_ICON: [&str; 3] = ["exe", "lnk", "ico"];

/// Shell icon lookups made (cache misses); tests read it.
pub static SHELL_CALLS: AtomicUsize = AtomicUsize::new(0);

type Key = (String, u32);

/// Why the shell gave no icon.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    /// Not ready yet (`E_PENDING`): try again, never cache.
    Pending,
    Failed(String),
}

type Source = fn(&str, u32) -> Result<(u32, u32, Vec<u8>), Lookup>;

/// Asks again this many times, this far apart, while the shell is busy.
const PENDING_TRIES: u32 = 10;
const PENDING_WAIT: std::time::Duration = std::time::Duration::from_millis(15);

pub struct Icons {
    cache: Mutex<HashMap<Key, Option<std::sync::Arc<Vec<u8>>>>>,
    source: Source,
}

impl Default for Icons {
    fn default() -> Self {
        Icons {
            cache: Mutex::default(),
            source: os::icon_bgra,
        }
    }
}

fn cache_key(path: &str, is_dir: bool, px: u32) -> Key {
    (icon_key(path, is_dir), px)
}

/// What a row's icon depends on: its type (".pdf"), its own path for
/// files that carry their own icon, or "<folder>". Rows with the same key
/// show the same icon, so the page asks for each key once.
pub fn icon_key(path: &str, is_dir: bool) -> String {
    if is_dir {
        return "<folder>".into();
    }
    let ext = std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if OWN_ICON.contains(&ext.as_str()) {
        path.to_lowercase()
    } else {
        format!(".{ext}")
    }
}

impl Icons {
    /// The icon of `path` as a `px`×`px` PNG; `None` when the shell has
    /// none (the row keeps its generic glyph).
    pub fn png(&self, path: &str, is_dir: bool, px: u32) -> Option<std::sync::Arc<Vec<u8>>> {
        let px = px.clamp(16, 256);
        let key = cache_key(path, is_dir, px);
        if let Some(hit) = self.cache.lock().ok()?.get(&key) {
            return hit.clone();
        }
        SHELL_CALLS.fetch_add(1, Ordering::Relaxed);
        let mut got = (self.source)(path, px);
        for _ in 1..PENDING_TRIES {
            if got != Err(Lookup::Pending) {
                break;
            }
            std::thread::sleep(PENDING_WAIT);
            got = (self.source)(path, px);
        }
        let made = match got {
            Ok((w, h, bgra)) => Some(std::sync::Arc::new(encode_png(w, h, &bgra))),
            Err(Lookup::Pending) => {
                // still busy: no icon this time, and not remembered as none
                crate::log::line(format!("shell icon still pending for {key:?}"));
                return None;
            }
            Err(Lookup::Failed(why)) => {
                crate::log::line(format!("no shell icon for {key:?}: {why}"));
                // a type's icon is read through one file of that type; if
                // that file is gone or unreadable, the next one may work
                if key.0.starts_with('.') {
                    return None;
                }
                None // the file's own icon: remembered as none
            }
        };
        if let Ok(mut c) = self.cache.lock() {
            c.insert(key, made.clone());
        }
        made
    }
}

/// Premultiplied top-down BGRA (what GDI hands back) to a PNG.
pub fn encode_png(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for p in bgra.as_chunks::<4>().0 {
        let a = p[3] as u32;
        let un = |c: u8| ((c as u32 * 255 + a / 2).checked_div(a).unwrap_or(0)).min(255) as u8;
        rgba.extend_from_slice(&[un(p[2]), un(p[1]), un(p[0]), p[3]]);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().expect("writing to a Vec");
        wr.write_image_data(&rgba).expect("writing to a Vec");
    }
    out
}

#[cfg(windows)]
mod os {
    use super::Lookup;
    use windows::Win32::Foundation::SIZE;

    /// "The data necessary to complete this operation is not yet
    /// available" (winerror.h E_PENDING).
    const E_PENDING: windows::core::HRESULT = windows::core::HRESULT(0x8000_000A_u32 as i32);
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDIBits, GetObjectW, HGDIOBJ,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    };
    use windows::core::HSTRING;

    /// The shell's icon for `path` at `px`, as (width, height, top-down
    /// premultiplied BGRA).
    pub fn icon_bgra(path: &str, px: u32) -> Result<(u32, u32, Vec<u8>), Lookup> {
        crate::recent::os::with_com(|| {
            // SAFETY: COM is initialised; the bitmap is deleted below and
            // the buffer is sized from the bitmap's own dimensions.
            unsafe {
                let item: IShellItemImageFactory =
                    SHCreateItemFromParsingName(&HSTRING::from(path), None)
                        .map_err(|e| Lookup::Failed(e.message()))?;
                let size = SIZE {
                    cx: px as i32,
                    cy: px as i32,
                };
                let hbm = item
                    .GetImage(size, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)
                    .map_err(|e| {
                        // the shell is still extracting the icon (first use of a
                        // type): ask again shortly
                        if e.code() == E_PENDING {
                            Lookup::Pending
                        } else {
                            Lookup::Failed(e.message())
                        }
                    })?;
                let mut bm = BITMAP::default();
                let got = GetObjectW(
                    HGDIOBJ(hbm.0),
                    std::mem::size_of::<BITMAP>() as i32,
                    Some((&mut bm as *mut BITMAP).cast()),
                );
                let (w, h) = (bm.bmWidth.unsigned_abs(), bm.bmHeight.unsigned_abs());
                let mut buf = vec![0u8; (w * h * 4) as usize];
                let mut info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w as i32,
                        biHeight: -(h as i32), // top-down rows
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let dc = CreateCompatibleDC(None);
                let lines = GetDIBits(
                    dc,
                    hbm,
                    0,
                    h,
                    Some(buf.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                );
                let _ = DeleteDC(dc);
                let _ = DeleteObject(HGDIOBJ(hbm.0));
                if got != 0 && lines == h as i32 && w > 0 {
                    Ok((w, h, buf))
                } else {
                    Err(Lookup::Failed("the icon bitmap could not be read".into()))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_key_names_what_the_icon_depends_on() {
        assert_eq!(icon_key(r"C:\a\Rapor.PDF", false), ".pdf");
        assert_eq!(icon_key(r"C:\b\other.pdf", false), ".pdf");
        assert_eq!(icon_key(r"C:\Apps\Tool.EXE", false), r"c:\apps\tool.exe");
        assert_eq!(icon_key(r"C:\a\Rapor", true), "<folder>");
        assert_eq!(icon_key(r"C:\a\README", false), ".");
        // an internet shortcut's icon can point at another computer: it
        // gets its type's icon, not its own
        assert_eq!(icon_key(r"C:\a\Site.url", false), ".url");
    }

    #[test]
    fn one_unreadable_file_does_not_cost_its_type_the_icon() {
        // the shell fails for the first pdf (gone, no access), then works
        fn first_fails(path: &str, _: u32) -> Result<(u32, u32, Vec<u8>), Lookup> {
            if path.ends_with("gone.pdf") {
                Err(Lookup::Failed("not found".into()))
            } else {
                Ok((1, 1, vec![0, 0, 255, 255]))
            }
        }
        let icons = Icons {
            cache: Mutex::default(),
            source: first_fails,
        };
        assert!(icons.png(r"C:\a\gone.pdf", false, 32).is_none());
        assert!(icons.png(r"C:\a\here.pdf", false, 32).is_some());
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("flint-icons-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn icon_cache_hits_by_extension() {
        let d = tmp("cache");
        for n in ["a.flinttest", "b.flinttest", "c.FLINTTEST"] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let icons = Icons::default();
        let before = SHELL_CALLS.load(Ordering::Relaxed);
        for n in ["a.flinttest", "b.flinttest", "c.FLINTTEST"] {
            icons.png(&d.join(n).to_string_lossy(), false, 32);
        }
        // other tests may run in parallel: count only this test's keys
        let c = icons.cache.lock().unwrap();
        assert_eq!(c.len(), 1, "one lookup for one extension");
        assert!(SHELL_CALLS.load(Ordering::Relaxed) > before);
        drop(c);
        // a program carries its own icon: two of them are two lookups
        let exe = std::env::current_exe().unwrap();
        let copy = d.join("kopya.exe");
        std::fs::copy(&exe, &copy).unwrap();
        icons.png(&exe.to_string_lossy(), false, 32);
        icons.png(&copy.to_string_lossy(), false, 32);
        icons.png(&d.to_string_lossy(), true, 32);
        icons.png(&tmp("cache2").to_string_lossy(), true, 32);
        assert_eq!(
            icons.cache.lock().unwrap().len(),
            4,
            "extension + 2 programs + folder"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn icon_png_is_valid() {
        let d = tmp("png");
        let f = d.join("rapor.txt");
        std::fs::write(&f, b"x").unwrap();
        let icons = Icons::default();
        let png_bytes = icons
            .png(&f.to_string_lossy(), false, 32)
            .expect("the shell has a .txt icon");
        assert_eq!(&png_bytes[..8], b"\x89PNG\r\n\x1a\n");
        let dec = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
        let r = dec.read_info().unwrap();
        let info = r.info();
        assert_eq!((info.width, info.height), (32, 32));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pending_icon_is_retried_and_never_cached_as_none() {
        use std::sync::atomic::AtomicU32;
        static CALLS: AtomicU32 = AtomicU32::new(0);
        // busy for the first 3 asks, then a 1x1 icon
        fn busy_then_ready(_: &str, _: u32) -> Result<(u32, u32, Vec<u8>), Lookup> {
            if CALLS.fetch_add(1, Ordering::SeqCst) < 3 {
                Err(Lookup::Pending)
            } else {
                Ok((1, 1, vec![0, 0, 255, 255]))
            }
        }
        let icons = Icons {
            cache: Mutex::default(),
            source: busy_then_ready,
        };
        assert!(
            icons.png(r"C:\a.pending", false, 32).is_some(),
            "retried until ready"
        );
        static NEVER: AtomicU32 = AtomicU32::new(0);
        fn always_busy(_: &str, _: u32) -> Result<(u32, u32, Vec<u8>), Lookup> {
            NEVER.fetch_add(1, Ordering::SeqCst);
            Err(Lookup::Pending)
        }
        let icons = Icons {
            cache: Mutex::default(),
            source: always_busy,
        };
        assert!(icons.png(r"C:\b.pending", false, 32).is_none());
        assert!(
            icons.cache.lock().unwrap().is_empty(),
            "a busy shell is not remembered as no icon"
        );
        assert_eq!(NEVER.load(Ordering::SeqCst), PENDING_TRIES);
    }

    #[test]
    fn unpremultiplies_alpha() {
        // half-transparent pure red, premultiplied: (B,G,R,A) = (0,0,128,128)
        let png_bytes = encode_png(1, 1, &[0, 0, 128, 128]);
        let mut r = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        let mut px = vec![0; r.output_buffer_size().unwrap()];
        r.next_frame(&mut px).unwrap();
        assert_eq!(&px[..4], &[255, 0, 0, 128]);
    }
}
