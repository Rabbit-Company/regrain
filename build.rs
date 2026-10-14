fn main() {
  println!("cargo:rustc-env=REGRAIN_TARGET={}", std::env::var("TARGET").unwrap_or_default());
  println!("cargo:rerun-if-changed=build.rs");
}
