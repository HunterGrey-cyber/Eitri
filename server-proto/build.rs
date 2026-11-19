use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Anchor generation and Cargo's input tracking to this checkout when targets are shared.
    let proto = Path::new(env!("CARGO_MANIFEST_DIR")).join("proto");
    let files = [proto.join("eitri_server.proto"), proto.join("eitri_terminal.proto")];
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    println!("cargo:rerun-if-changed={}", proto.display());
    tonic_prost_build::configure().compile_protos(&files, &[proto])?;
    Ok(())
}
