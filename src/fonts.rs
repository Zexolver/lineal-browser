//! Fallback fonts on Linux, without fontconfig.
//!
//! Pages are drawn with the bundled monospace font, but text in other scripts (Chinese, Arabic,
//! emoji…) needs a font that has those glyphs: the terminal draws the actual characters, we just
//! need to know which character each glyph is. On Windows, macOS and Android, fontique finds
//! system fonts natively. On Linux it would link the C library fontconfig, which rules out fully
//! static binaries, so instead we memory-map a few well-known fallback fonts from the usual font
//! folders and register them as fallbacks for every script.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parley::fontique::{Blob, Collection, FamilyId, Script};

/// Font files whose names contain one of these are fallback candidates (wide coverage fonts).
const WANTED: &[&str] = &[
    "cjk",
    "emoji",
    "symbola",
    "unifont",
    "droidsansfallback",
    "wqy",
    "freesans",
    "freemono",
    "freeserif",
    "dejavusans",
    "notosans",
    "notoserifcjk",
    "notomono",
];

/// Skip non-regular styles: one face per family is enough to know which character a glyph is.
const STYLES: &[&str] = &[
    "bold",
    "italic",
    "oblique",
    "light",
    "thin",
    "black",
    "medium",
    "semibold",
    "condensed",
    "extra",
    "heavy",
    "display",
    "ui-",
    "ui.",
];

const MAX_FILES: usize = 80;

/// Scripts we register fallbacks for (ISO 15924).
const SCRIPTS: &[&[u8; 4]] = &[
    b"Zyyy", b"Zinh", b"Zsym", b"Zsye", b"Latn", b"Grek", b"Cyrl", b"Armn", b"Hebr", b"Arab", b"Syrc", b"Thaa",
    b"Deva", b"Beng", b"Guru", b"Gujr", b"Orya", b"Taml", b"Telu", b"Knda", b"Mlym", b"Sinh", b"Thai", b"Laoo",
    b"Tibt", b"Mymr", b"Geor", b"Hang", b"Ethi", b"Cher", b"Khmr", b"Mong", b"Hira", b"Kana", b"Bopo", b"Hani",
    b"Yiii", b"Copt", b"Brai",
];

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/share/fonts"), PathBuf::from("/usr/local/share/fonts")];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/fonts"));
        dirs.push(home.join(".fonts"));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data).join("fonts"));
    }
    // Termux keeps its files under $PREFIX.
    if let Some(prefix) = std::env::var_os("PREFIX") {
        dirs.push(PathBuf::from(prefix).join("share/fonts"));
    }
    dirs
}

fn find(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                find(&path, depth - 1, out);
            }
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        let font = [".ttf", ".otf", ".ttc"].iter().any(|e| name.ends_with(e));
        if font && WANTED.iter().any(|w| name.contains(w)) && !STYLES.iter().any(|s| name.contains(s)) {
            out.push(path);
        }
    }
}

/// Registers fallback fonts found on this system. Returns how many files were used.
pub fn register_fallbacks(collection: &mut Collection) -> usize {
    let mut files = Vec::new();
    for dir in font_dirs() {
        find(&dir, 4, &mut files);
    }
    files.sort();
    files.dedup();
    // Prefer the broadest fonts when there are too many.
    let rank = |p: &PathBuf| {
        let n = p.to_string_lossy().to_ascii_lowercase();
        if n.contains("cjk") || n.contains("emoji") || n.contains("unifont") { 0 } else { 1 }
    };
    files.sort_by_key(rank);
    files.truncate(MAX_FILES);

    let mut cjk: Vec<FamilyId> = Vec::new();
    let mut emoji: Vec<FamilyId> = Vec::new();
    let mut other: Vec<FamilyId> = Vec::new();
    for path in &files {
        let Ok(file) = std::fs::File::open(path) else { continue };
        // SAFETY: font files are mapped read-only and not expected to change while we run
        // (fontique maps system fonts the same way).
        let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else { continue };
        let name = path.to_string_lossy().to_ascii_lowercase();
        let ids = collection.register_fonts(Blob::new(Arc::new(map)), None).into_iter().map(|(id, _)| id);
        let bucket = if name.contains("emoji") {
            &mut emoji
        } else if name.contains("cjk") || name.contains("wqy") || name.contains("droidsansfallback") {
            &mut cjk
        } else {
            &mut other
        };
        for id in ids {
            if !bucket.contains(&id) {
                bucket.push(id);
            }
        }
    }
    for script in SCRIPTS {
        let s = Script::from_bytes(**script);
        let order: Vec<FamilyId> = match *script {
            b"Hani" | b"Hira" | b"Kana" | b"Bopo" | b"Hang" | b"Yiii" => {
                cjk.iter().chain(&other).chain(&emoji).copied().collect()
            }
            b"Zsye" | b"Zsym" => emoji.iter().chain(&other).chain(&cjk).copied().collect(),
            _ => other.iter().chain(&cjk).chain(&emoji).copied().collect(),
        };
        collection.append_fallbacks(s, order.into_iter());
    }
    files.len()
}
