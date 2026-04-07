fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("../hugimq/proto/hugimq.proto")?;
    Ok(())
}
