fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true) // Client used only in #[cfg(test)] integration tests
        .compile_protos(&["../shared/proto/dexter.proto"], &["../shared/proto/"])?;
    Ok(())
}
