fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)   // Client used only in #[cfg(test)] integration tests
        .compile_protos(
            &["../shared/proto/dexter.proto"],
            &["../shared/proto/"],
        )?;
    Ok(())
}
