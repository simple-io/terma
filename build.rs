// build.rs — 编译 Slint DSL 文件进二进制
//
// 职责：
//   1. 编译 src/ui/main_window.slint 进二进制
//   2. 监控 src/ui/ 目录变化触发重新编译
//   3. 生成构建时间戳（BUILD_TIMESTAMP，UTC）供「关于」窗口展示

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=src/ui/");

    // 构建时间戳：build.rs 每次运行都会刷新，反映最近一次实际编译时刻
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", format_utc(secs));

    slint_build::compile_with_config(
        "src/ui/main_window.slint",
        slint_build::CompilerConfiguration::new(),
    )
    .expect("Slint UI compilation failed");
}

/// 将 Unix 秒数格式化为 `YYYY-MM-DD HH:MM:SS UTC`（Howard indent civil_from_days 算法）。
fn format_utc(total_secs: u64) -> String {
    let days = (total_secs / 86_400) as i64;
    let rem = total_secs % 86_400;
    let (hh, mm, ss) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    if mo <= 2 {
        y += 1;
    }
    format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}
