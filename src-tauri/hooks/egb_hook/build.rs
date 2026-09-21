fn main() {
    // seh.c：MSVC 的 __try/__except 无法用 stable Rust 表达，单独编成 C 对象
    cc::Build::new().file("src/seh.c").compile("egb_seh");
    println!("cargo:rerun-if-changed=src/seh.c");
}
