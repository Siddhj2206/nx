//! Links the system librpm and librpmio (ADR-0008).
fn main() {
    println!("cargo:rustc-link-lib=dylib=rpm");
    println!("cargo:rustc-link-lib=dylib=rpmio");
}
