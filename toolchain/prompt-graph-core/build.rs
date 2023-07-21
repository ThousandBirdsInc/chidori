fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=./protobufs/DSL_v1.proto");

    #[cfg(feature = "build-protos")]
    tonic_build::configure()
        .out_dir("./src/generated_protobufs")
        .build_server(true)
        .type_attribute(".", "#[derive(serde::Deserialize, serde::Serialize)]") // adding attributes
        .type_attribute("promptgraph.ExecutionStatus", "#[derive(typescript_type_def::TypeDef)]") // adding attributes
        .compile(&["./protobufs/DSL_v1.proto"], &["./protobufs/"])
        .unwrap_or_else(|e| panic!("protobuf compile error: {}", e));

    Ok(())
}