// build.rs — 编译 Slint DSL 文件进二进制
//
// 职责：
//   1. 编译 src/ui/main_window.slint 进二进制
//   2. 监控 src/ui/ 目录变化触发重新编译

fn main() {
    println!("cargo:rerun-if-changed=src/ui/");

    slint_build::compile_with_config(
        "src/ui/main_window.slint",
        slint_build::CompilerConfiguration::new(),
    )
    .expect("Slint UI compilation failed");
}
