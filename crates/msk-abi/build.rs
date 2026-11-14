fn main() {
    cc::Build::new()
        .file("include/msk_abi_layout.c")
        .include("include")
        .warnings(false)
        .compile("msk_abi_layout");
    println!("cargo:rerun-if-changed=include/msk_abi.h");
    println!("cargo:rerun-if-changed=include/msk_abi_layout.c");
}
