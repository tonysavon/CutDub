fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons/cutdub.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("../../assets/icons/cutdub.ico")
            .set("ProductName", "CutDub")
            .set("FileDescription", "CutDub — Precision Video Processor")
            .set("OriginalFilename", "cutdub.exe")
            .compile()
            .expect("Failed to embed the CutDub Windows icon");
    }
}
