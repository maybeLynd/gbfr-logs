fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/master-trait-effects.rs");
    let res = winres::WindowsResource::new();
    res.compile().unwrap();
}
