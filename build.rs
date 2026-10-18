fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        if std::path::Path::new("assets/lumen.ico").exists() {
            res.set_icon("assets/lumen.ico");
        }
        res.set("FileDescription", "Lumen Image & Video Viewer");
        res.set("ProductName", "Lumen");
        res.set("LegalCopyright", "MIT License");
        res.set_manifest_file("assets/lumen.manifest");
        if let Err(e) = res.compile() {
            println!("cargo:warning=resource compile failed: {e}");
        }
        // Static FFmpeg (video feature) needs these Windows system libraries.
        if std::env::var_os("CARGO_FEATURE_VIDEO").is_some() {
            for lib in [
                "mfplat", "mfuuid", "strmiids", "ole32", "oleaut32", "user32", "gdi32", "shlwapi", "advapi32", "bcrypt",
                "secur32", "ws2_32", "psapi", "crypt32", "ncrypt",
            ] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
    }
    println!("cargo:rerun-if-changed=assets/lumen.ico");
    println!("cargo:rerun-if-changed=assets/lumen.manifest");
}
