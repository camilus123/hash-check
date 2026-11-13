//! Fallback fonts: characters common in file names must find a font.

use hash_check::fonts::FileFontProvider;
use std::path::PathBuf;

/// On Windows the real provider must cover what download tools put into
/// file names (full-width punctuation, the big solidus) and CJK text.
#[cfg(windows)]
#[test]
fn windows_fonts_cover_characters_seen_in_file_names() {
    let provider =
        FileFontProvider::for_this_system().expect("Windows needs the fallback provider");
    let mut missing = Vec::new();
    for c in [
        '：', '？', '｜', '＂', '＊', '＜', '＞', '⧸', '⧹', '→', '™', '日', '한', 'я', '√',
    ] {
        match provider.find(c) {
            Some((file, face, _)) => println!(
                "{c} U+{:04X}: {} face {face}",
                c as u32,
                provider.file(file).display()
            ),
            None => missing.push(format!("{c} U+{:04X}", c as u32)),
        }
    }
    assert!(
        missing.is_empty(),
        "no Windows font found for: {}",
        missing.join(", ")
    );
}

/// The lookup itself, on whatever font this machine has.
#[test]
fn finds_glyphs_in_font_files_and_skips_unusable_ones() {
    let candidates = [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
    ];
    let Some(real) = candidates.iter().map(PathBuf::from).find(|p| p.is_file()) else {
        eprintln!("no known font on this machine; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let garbage = dir.path().join("not-a-font.ttf");
    std::fs::write(&garbage, b"this is not a font").unwrap();
    let provider = FileFontProvider::new(vec![dir.path().join("missing.ttf"), garbage, real]);

    let (file, _face, bytes) = provider.find('A').expect("a Latin font must have 'A'");
    assert_eq!(file, 2, "missing and broken files are skipped");
    assert!(bytes.len() > 1000);
    // A private-use code point no ordinary font defines.
    assert!(provider.find('\u{F8F0}').is_none());
    // Asking again reuses the loaded file.
    assert!(provider.find('B').is_some());
}
