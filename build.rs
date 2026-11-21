fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "EzThumb");
        res.set(
            "FileDescription",
            "EzThumb - Video thumbnail contact-sheet generator powered by FFmpeg",
        );
        res.set("OriginalFilename", "ezthumb.exe");
        res.set(
            "LegalCopyright",
            "Copyright (C) 2011-2024 Andy Xuming, 2026 Aaron Christopher Ramos",
        );
        res.compile()
            .expect("Failed to compile Windows icon resource.");
    }
}
