fn main() {
    println!("cargo:rerun-if-changed=src/cgvirtual.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/cgvirtual.m")
            .flag("-fobjc-arc")
            .flag("-fobjc-arc-exceptions")
            .flag("-fobjc-exceptions")
            .warnings_into_errors(true)
            .compile("intendant_cgvirtual");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
    }
}
