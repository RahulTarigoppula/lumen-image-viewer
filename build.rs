fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        if std::path::Path::new("assets/lumen.ico").exists() {
            res.set_icon("assets/lumen.ico");
        }
        res.set("FileDescription", "Lumen Image Viewer");
        res.set("ProductName", "Lumen");
        res.set_manifest_file("assets/lumen.manifest");
        if let Err(e) = res.compile() {
            println!("cargo:warning=resource compile failed: {e}");
        }
    }
}
