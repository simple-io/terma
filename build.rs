// build.rs — 编译 Slint DSL 文件进二进制
// 负责把 src/ui/main_window.slint 编译为 Rust 代码，
// main_window.slint 通过 import 引入 settings_window.slint 等其他 UI 文件，
// 全部在同一个编译单元中生成。之后在 Rust 侧通过 slint::include_modules!() 引入。
fn main() {
    slint_build::compile("src/ui/main_window.slint").expect("Slint UI compilation failed");
}
