fn main() {
    // Compile the Objective-C shim. macOS only; nothing to build elsewhere.
    //
    //  - open_shim.m: the Open With application delegate (see src/open.rs).
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rerun-if-changed=src/open_shim.m");
        cc::Build::new()
            .file("src/open_shim.m")
            .flag("-fobjc-arc")
            .compile("ab_shims");
        // objc2-app-kit already links AppKit, but be explicit so the shim's
        // symbols resolve regardless of link order.
        println!("cargo:rustc-link-lib=framework=AppKit");
    }
}
