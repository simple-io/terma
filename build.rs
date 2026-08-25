// build.rs — 编译 Slint DSL 文件进二进制
// 负责把 src/ui/main_window.slint 编译为 Rust 代码，
// 之后在 Rust 侧通过 slint::include_modules!() 引入生成的 MainWindow 类型。
fn main() {
    slint_build::compile("src/ui/main_window.slint")
        .expect("Slint UI compilation failed");
}
