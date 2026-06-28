fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false) // CLI doesn't need server code
        .build_client(true)
        .compile_protos(
            &["../../proto/bosun/v1/bosun.proto"],
            &["../../proto"], // include path from workspace root
        )?;
    Ok(())
}
