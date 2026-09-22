fn main() {
    println!("cargo:rerun-if-changed=src/cgvirtual.m");
    println!("cargo:rerun-if-changed=src/bound_pointer.m");
    println!("cargo:rerun-if-changed=src/bound_arrow.m");
    println!("cargo:rerun-if-changed=src/foreground.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/cgvirtual.m")
            .file("src/bound_pointer.m")
            .file("src/bound_arrow.m")
            .file("src/foreground.m")
            .flag("-fobjc-arc")
            .flag("-fobjc-arc-exceptions")
            .flag("-fobjc-exceptions")
            .warnings_into_errors(true)
            .compile("intendant_cgvirtual");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
    }
}
