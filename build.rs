use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=aidl");
    let out_dir = env::var("OUT_DIR").unwrap();
    let out_file = PathBuf::from(&out_dir).join("light.rs");

    rsbinder_aidl::Builder::new()
        .source(&PathBuf::from("aidl"))
        .output(&out_file)
        .generate()
        .expect("failed to generate light AIDL bindings");

    // Same fixups as the other HyperOS mocks: AIDL metadata transactions are
    // hard-coded by libbinder and the generated code needs to accept them.
    let generated = fs::read_to_string(&out_file).expect("read generated bindings");
    let patched = generated
        .lines()
        .map(|line| {
            if line.contains("const r#getInterfaceVersion:") {
                "                        pub(crate) const r#getInterfaceVersion: rsbinder::TransactionCode = 16777215;".to_string()
            } else if line.contains("const r#getInterfaceHash:") {
                "                        pub(crate) const r#getInterfaceHash: rsbinder::TransactionCode = 16777214;".to_string()
            } else if line.contains("#![allow(non_upper_case_globals, non_snake_case, dead_code)]") {
                line.replace("dead_code", "dead_code, non_camel_case_types")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&out_file, patched).expect("write patched bindings");
}
