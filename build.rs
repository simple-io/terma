// build.rs — 编译 Slint DSL 文件进二进制 + 静态打包 fonts/ 目录内置字体
//
// 职责：
//   1. 扫描 <manifest>/fonts/ 下的 .ttf/.otf 字体文件，解析每个字体的 family 名
//   2. 生成 $OUT_DIR/bundled_fonts.slint：通过 Slint 的字体 import 语句
//      （import "/abs/path/font.ttf"）让 Slint 编译器把字体以 FileData 形式
//      静态内嵌进二进制（默认 EmbedAllResources），字体 family 全局可用于
//      font-family 属性
//   3. 生成 $OUT_DIR/bundled_font_names.rs：内置字体 family 名列表，
//      供 Rust 侧设置界面下拉框使用（与 .slint 内嵌的字体保持同一数据源）
//   4. 把 $OUT_DIR 加入 Slint 编译 include path，编译 src/ui/main_window.slint
//
// 字体目录内容变化（新增/删除/修改字体）会触发构建脚本重新执行。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=fonts/");
    println!("cargo:rerun-if-changed=src/ui/");
    println!("cargo:rerun-if-changed=build.rs");

    let bundled = scan_bundled_fonts(&manifest_dir);
    generate_bundled_fonts_slint(&out_dir, &manifest_dir, &bundled);
    generate_bundled_font_names_rs(&out_dir, &bundled);

    // 编译 Slint UI：附加 OUT_DIR 到 include path，
    // 使 main_window.slint 中的 `import { BundledFonts } from "bundled_fonts.slint"` 可解析
    let config = slint_build::CompilerConfiguration::new()
        .with_include_paths(vec![out_dir.clone()]);
    slint_build::compile_with_config("src/ui/main_window.slint", config)
        .expect("Slint UI compilation failed");
}

/// 内置字体条目：字体文件绝对路径 + 解析出的 family 名
struct BundledFontEntry {
    path: PathBuf,
    family: String,
}

/// 扫描 fonts/ 目录（按文件名排序保证构建可复现）。
fn scan_bundled_fonts(manifest_dir: &Path) -> Vec<BundledFontEntry> {
    let fonts_dir = manifest_dir.join("fonts");
    let mut files: Vec<PathBuf> = Vec::new();
    if fonts_dir.is_dir() {
        if let Ok(rd) = fs::read_dir(&fonts_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_ascii_lowercase());
                if matches!(ext.as_deref(), Some("ttf") | Some("otf")) {
                    files.push(path);
                }
            }
        }
    }
    files.sort();

    files
        .into_iter()
        .map(|path| {
            let family = fs::read(&path)
                .ok()
                .and_then(|data| parse_font_family(&data))
                .unwrap_or_else(|| {
                    // 解析失败时退化为文件名主干，保证下拉框仍有条目可展示
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Unknown")
                        .to_string()
                });
            BundledFontEntry { path, family }
        })
        .collect()
}

/// 生成 bundled_fonts.slint：字体 import 语句（绝对路径，仅编译期使用）。
/// main_window.slint 通过 `import { BundledFonts } from "bundled_fonts.slint"` 引入，
/// Slint 编译器收集所有文档的字体 import 并在组件初始化时注册（内嵌进二进制）。
fn generate_bundled_fonts_slint(out_dir: &Path, manifest_dir: &Path, bundled: &[BundledFontEntry]) {
    let mut code = String::from("// 由 build.rs 自动生成，勿手动编辑。\n");
    for font in bundled {
        // 用绝对路径引用字体文件（typeloader 直接解析绝对路径）
        let path = font
            .path
            .strip_prefix(manifest_dir)
            .unwrap_or(&font.path)
            .to_string_lossy()
            .replace('\\', "/");
        let abs = manifest_dir.join(&path).to_string_lossy().replace('\\', "/");
        code.push_str(&format!("import \"{}\";\n", abs));
    }
    // 导出空组件供 main_window.slint import，使本文件进入编译单元
    code.push_str("\nexport component BundledFonts {\n}\n");
    fs::write(out_dir.join("bundled_fonts.slint"), code).expect("写入 bundled_fonts.slint 失败");
}

/// 生成 bundled_font_names.rs：去重后的内置字体 family 名列表。
fn generate_bundled_font_names_rs(out_dir: &Path, bundled: &[BundledFontEntry]) {
    let mut families: Vec<String> = Vec::new();
    for font in bundled {
        if !families.iter().any(|f| f.eq_ignore_ascii_case(&font.family)) {
            families.push(font.family.clone());
        }
    }
    let mut code = String::from("// 由 build.rs 自动生成，勿手动编辑。\n");
    code.push_str("/// 内置字体 family 名（按打包顺序去重）\n");
    code.push_str("pub static BUNDLED_FONT_FAMILIES: &[&str] = &[\n");
    for family in &families {
        code.push_str(&format!("    {:?},\n", family));
    }
    code.push_str("];\n");
    fs::write(out_dir.join("bundled_font_names.rs"), code)
        .expect("写入 bundled_font_names.rs 失败");
}

// ─── TTF/OTF name 表解析（仅提取 family 名，name ID = 1） ───
//
// sfnt 结构：u32 magic | u16 numTables | ...，表记录从偏移 12 开始（每条 16 字节）。
// name 表记录：platformID | encodingID | languageID | nameID | length | offset（各 u16），
// 字符串数据从 name 表头 stringOffset 起算。优先取 Windows Unicode (3,1) 的 UTF-16BE。

fn parse_font_family(data: &[u8]) -> Option<String> {
    if data.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;

    // 定位 name 表
    let mut name_off = 0usize;
    let mut name_len = 0usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if rec + 16 > data.len() {
            return None;
        }
        if &data[rec..rec + 4] == b"name" {
            name_off = u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]]) as usize;
            name_len = u32::from_be_bytes([data[rec + 12], data[rec + 13], data[rec + 14], data[rec + 15]]) as usize;
        }
    }
    if name_off == 0 {
        return None;
    }
    let table = data.get(name_off..name_off + name_len)?;

    if table.len() < 6 {
        return None;
    }
    let count = u16::from_be_bytes([table[2], table[3]]) as usize;
    let string_off = u16::from_be_bytes([table[4], table[5]]) as usize;

    let mut best: Option<String> = None;
    let mut best_score = 0u8;
    for i in 0..count {
        let rec = 6 + i * 12;
        if rec + 12 > table.len() {
            break;
        }
        let platform = u16::from_be_bytes([table[rec], table[rec + 1]]);
        let encoding = u16::from_be_bytes([table[rec + 2], table[rec + 3]]);
        let name_id = u16::from_be_bytes([table[rec + 6], table[rec + 7]]);
        let length = u16::from_be_bytes([table[rec + 8], table[rec + 9]]) as usize;
        let offset = u16::from_be_bytes([table[rec + 10], table[rec + 11]]) as usize;
        if name_id != 1 {
            continue;
        }
        let str_data = table.get(string_off + offset..string_off + offset + length)?;
        let s = match (platform, encoding) {
            (3, 1) => decode_utf16be(str_data), // Windows Unicode BMP
            (0, _) => decode_utf16be(str_data), // Unicode platform
            (1, 0) => str_data.iter().map(|&b| b as char).collect(), // Mac Roman
            _ => continue,
        };
        // 优先级：Windows Unicode > Unicode platform > Mac Roman
        let score = match (platform, encoding) {
            (3, 1) => 3,
            (0, _) => 2,
            _ => 1,
        };
        if score > best_score {
            best = Some(s);
            best_score = score;
        }
    }
    best.filter(|s| !s.is_empty())
}

/// UTF-16BE 解码（含代理对）
fn decode_utf16be(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).unwrap_or_default()
}
