fn main() {
    println!("cargo:rerun-if-changed=native/vt_frc.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("native/vt_frc.m")
        .flag("-fobjc-arc")
        // Match the app's deployment floor so the newer classes are weak imports.
        .flag("-mmacosx-version-min=11.0")
        .opt_level(2)
        .compile("vt_frc");
    for framework in ["Foundation", "VideoToolbox", "CoreVideo", "CoreMedia"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
