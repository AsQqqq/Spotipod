// Compiles protos librespot-protocol doesn't generate.
fn main() {
    println!("cargo:rerun-if-changed=proto");
    protobuf_codegen::Codegen::new()
        .pure()
        .cargo_out_dir("proto")
        .include("proto")
        .input("proto/collection2v2.proto")
        .run_from_script();
}
