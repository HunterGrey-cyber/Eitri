fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/eitri_server.proto");
    println!("cargo:rerun-if-changed=proto");
    tonic_prost_build::configure().compile_protos(&["proto/eitri_server.proto"], &["proto"])?;
    Ok(())
}
