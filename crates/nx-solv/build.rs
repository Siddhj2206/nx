//! Links the system libsolv and libsolvext (ADR-0007).
fn main() {
    println!("cargo:rustc-link-lib=dylib=solvext");
    println!("cargo:rustc-link-lib=dylib=solv");
}
