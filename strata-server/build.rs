// Compiles proto/strata.proto with protox, so no protoc is needed.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/strata.proto");
    let fds = protox::compile(["proto/strata.proto"], ["proto"])?;
    tonic_build::configure().compile_fds(fds)?;
    Ok(())
}
