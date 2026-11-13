//! Fallback fonts for characters the built-in font cannot draw.
//!
//! The GUI library's own system-font lookup picks fonts by script, which
//! works for Chinese or Korean text but not for "script-neutral" characters
//! such as the full-width punctuation that download tools put into file
//! names ("：", "？", "｜") or symbols like "⧸". On Windows those fall
//! through and show as empty boxes. This provider is asked after the
//! library's own: it looks through a fixed list of fonts that ship with
//! Windows and returns the first one that has the character.
//!
//! Font files are read through `fsread` (read-only), each at most once.

use crate::fsread;
use eframe::egui;
use egui::epaint::text::{
    FallbackRequest, FontInsert, FontPriority, FontProvider, InsertFontFamily,
};
use skrifa::MetadataProvider as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_FONT_BYTES: u64 = 96 * 1024 * 1024;

/// Fonts that ship with Windows 10/11, most useful first: symbols,
/// Chinese (which carries the full-width forms), Japanese, Korean, maths,
/// then other scripts.
const WINDOWS_FONTS: &[&str] = &[
    "seguisym.ttf",
    "segoeui.ttf",
    "msyh.ttc",
    "msjh.ttc",
    "YuGothR.ttc",
    "msgothic.ttc",
    "malgun.ttf",
    "simsun.ttc",
    "cambria.ttc",
    "seguiemj.ttf",
    "seguihis.ttf",
    "Nirmala.ttc",
    "Nirmala.ttf",
    "ebrima.ttf",
    "gadugi.ttf",
    "leelawui.ttf",
    "micross.ttf",
    "arial.ttf",
    "times.ttf",
];

/// A font file, loaded on first use.
enum Loaded {
    NotYet,
    Unavailable,
    Bytes(Arc<Vec<u8>>),
}

pub struct FileFontProvider {
    files: Vec<PathBuf>,
    loaded: Mutex<Vec<Loaded>>,
}

impl FileFontProvider {
    pub fn new(files: Vec<PathBuf>) -> FileFontProvider {
        let loaded = files.iter().map(|_| Loaded::NotYet).collect();
        FileFontProvider {
            files,
            loaded: Mutex::new(loaded),
        }
    }

    /// The provider for this operating system, if it needs one.
    pub fn for_this_system() -> Option<FileFontProvider> {
        let files = system_font_files();
        if files.is_empty() {
            None
        } else {
            Some(FileFontProvider::new(files))
        }
    }

    /// The first font (file index, face index, bytes) that can draw `c`.
    pub fn find(&self, c: char) -> Option<(usize, u32, Arc<Vec<u8>>)> {
        let mut loaded = self.loaded.lock().unwrap();
        for (i, path) in self.files.iter().enumerate() {
            if matches!(loaded[i], Loaded::NotYet) {
                loaded[i] = match fsread::read_whole(path, MAX_FONT_BYTES) {
                    Ok(bytes) => Loaded::Bytes(Arc::new(bytes)),
                    Err(_) => Loaded::Unavailable,
                };
            }
            if let Loaded::Bytes(bytes) = &loaded[i]
                && let Some(face) = face_with_glyph(bytes, c)
            {
                return Some((i, face, bytes.clone()));
            }
        }
        None
    }

    pub fn file(&self, index: usize) -> &Path {
        &self.files[index]
    }
}

/// Index of the first face in a font file (or collection) that has a
/// drawable glyph for `c`.
fn face_with_glyph(bytes: &[u8], c: char) -> Option<u32> {
    let faces = match skrifa::raw::FileRef::new(bytes).ok()? {
        skrifa::raw::FileRef::Font(_) => 1,
        skrifa::raw::FileRef::Collection(col) => col.len(),
    };
    (0..faces).find(|&face| {
        let Ok(font) = skrifa::FontRef::from_index(bytes, face) else {
            return false;
        };
        match font.charmap().map(c) {
            Some(glyph) if glyph != skrifa::GlyphId::NOTDEF => {
                font.outline_glyphs().get(glyph).is_some()
                    || font.color_glyphs().get(glyph).is_some()
            }
            _ => false,
        }
    })
}

impl FontProvider for FileFontProvider {
    fn font_for(&self, request: &FallbackRequest<'_>) -> Option<FontInsert> {
        let c = request.cluster.chars().next()?;
        let (file, face, bytes) = self.find(c)?;
        let mut data = egui::FontData::from_owned(bytes.as_ref().clone());
        data.index = face;
        Some(FontInsert {
            name: format!("file:{}:{face}", self.files[file].display()),
            data,
            families: vec![InsertFontFamily {
                family: request.family.clone(),
                priority: FontPriority::Lowest,
            }],
        })
    }
}

fn system_font_files() -> Vec<PathBuf> {
    if cfg!(windows) {
        let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        let dir = PathBuf::from(windir).join("Fonts");
        WINDOWS_FONTS.iter().map(|f| dir.join(f)).collect()
    } else {
        // Elsewhere the library's own lookup already covers these characters.
        Vec::new()
    }
}

/// Install the fallback provider (after the GUI library's own).
pub fn install(ctx: &egui::Context) {
    if let Some(provider) = FileFontProvider::for_this_system() {
        ctx.add_font_provider(Arc::new(provider));
    }
}
