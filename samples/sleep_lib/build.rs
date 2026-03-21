fn main() {
    println!("cargo:rustc-link-search=native=../b_lib/target/release");
    println!("cargo:rustc-link-lib=dylib=b_lib");
}
