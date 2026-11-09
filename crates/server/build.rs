//! Embeds the app icon and version info into the Windows executable, and the built
//! web app (`web/dist`, see `web/README.md`) so the server can serve it.

const DESCRIPTION: &str = "Celerity Server";

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/icons/celerity.ico");
    embed_web_app();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/icons/celerity.ico");
        res.set("ProductName", "Celerity");
        res.set("FileDescription", DESCRIPTION);
        if let Err(e) = res.compile() {
            // A missing resource compiler shouldn't stop the build; the exe just has no icon.
            println!("cargo:warning=could not embed Windows icon: {e}");
        }
    }
}

/// Write `$OUT_DIR/web_files.rs`: every file in `web/dist` as `(url path, bytes)`.
/// Without a built web app the list is empty and the server says so at `/`.
fn embed_web_app() {
    use std::fmt::Write;
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files = Vec::new();
    collect(&dist, &dist, &mut files);
    files.sort();
    let mut out = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (url, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let _ = writeln!(out, "    ({url:?}, include_bytes!({:?})),", path.display().to_string());
    }
    out.push_str("];\n");
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("web_files.rs");
    std::fs::write(dest, out).expect("write web_files.rs");
}

fn collect(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, std::path::PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let url = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
            out.push((url, std::fs::canonicalize(&path).unwrap_or(path)));
        }
    }
}
