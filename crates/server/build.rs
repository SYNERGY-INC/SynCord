//! Embeds the app icon and version info into the Windows executable.

const DESCRIPTION: &str = "SynCord Server";

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/icons/syncord.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/icons/syncord.ico");
        res.set("ProductName", "SynCord");
        res.set("FileDescription", DESCRIPTION);
        if let Err(e) = res.compile() {
            // A missing resource compiler shouldn't stop the build; the exe just has no icon.
            println!("cargo:warning=could not embed Windows icon: {e}");
        }
    }
}
