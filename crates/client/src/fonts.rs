//! The UI font: Inter by default, or any font installed on the computer, or a font file
//! the user picked. The choice is applied when Celerity starts.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use iced::Font;

use crate::settings::Settings;
use crate::style::INTER;

/// Family name of the font chosen at startup.
static FAMILY: OnceLock<&'static str> = OnceLock::new();

/// The UI font for this run of Celerity.
pub fn ui() -> Font {
    FAMILY.get().map(|f| Font::with_name(f)).unwrap_or(INTER)
}

/// Pick the font for this run from the settings. Returns the default font and, for a font
/// file, its bytes to load. Call once, before the window opens.
pub fn init(settings: &Settings) -> (Font, Option<Vec<u8>>) {
    let mut data = None;
    let mut family = settings.ui_font.trim().to_string();
    if let Some(path) = &settings.font_file {
        match std::fs::read(path) {
            Ok(bytes) => data = Some(bytes),
            Err(e) => {
                eprintln!("couldn't load font {}: {e}", path.display());
                family.clear();
            }
        }
    }
    if family.is_empty() {
        return (INTER, None);
    }
    // Font names must live for the whole program; this happens once.
    let name: &'static str = Box::leak(family.into_boxed_str());
    let _ = FAMILY.set(name);
    (Font::with_name(name), data)
}

/// Family names of the fonts installed on this computer, sorted, without duplicates.
pub async fn installed() -> Vec<String> {
    tokio::task::spawn_blocking(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let mut names: Vec<String> =
            db.faces().filter_map(|f| f.families.first().map(|(name, _)| name.clone())).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        // Symbol and emoji fonts make the whole UI unreadable.
        names.retain(|n| {
            let l = n.to_lowercase();
            !["emoji", "symbol", "dingbat", "webdings", "wingdings", "icons"].iter().any(|bad| l.contains(bad))
        });
        names
    })
    .await
    .unwrap_or_default()
}

/// Folder where imported font files are kept.
fn fonts_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("celerity").join("fonts"))
}

/// Copy a font file into Celerity's settings folder. Returns its family name and new path.
pub fn import(path: &Path) -> Result<(String, PathBuf), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read the font: {e}"))?;
    let mut db = fontdb::Database::new();
    db.load_font_data(bytes.clone());
    let family = db
        .faces()
        .find_map(|f| f.families.first().map(|(name, _)| name.clone()))
        .ok_or("That file isn't a font Celerity can use (try a .ttf or .otf file).")?;
    let dir = fonts_dir().ok_or("Custom fonts aren't supported on this system.")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = path.file_name().ok_or("That file has no name")?;
    let dest = dir.join(name);
    std::fs::write(&dest, bytes).map_err(|e| format!("Couldn't save the font: {e}"))?;
    Ok((family, dest))
}

/// Start a new copy of Celerity (the caller then closes this one).
pub fn relaunch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe).spawn().map(|_| ()).map_err(|e| format!("Couldn't restart Celerity: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_a_font_file_and_reads_its_name() {
        let dir = std::env::temp_dir().join(format!("celerity-font-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("Inter-Regular.otf");
        std::fs::write(&src, include_bytes!("../assets/fonts/Inter-Regular.otf")).unwrap();
        // Only check reading the name; `import` also writes to the real settings folder.
        let mut db = fontdb::Database::new();
        db.load_font_data(std::fs::read(&src).unwrap());
        assert_eq!(db.faces().next().unwrap().families[0].0, "Inter");
        let bad = dir.join("notes.ttf");
        std::fs::write(&bad, b"not a font").unwrap();
        assert!(import(&bad).unwrap_err().contains("isn't a font"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
