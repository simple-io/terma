//! 关于窗口（AboutWindow）相关逻辑：懒创建、软件信息注入、仓库地址打开。
//!
//! 作为 `App`（controller）的子模块，复用主控制器的 event_tx。
//! 软件信息来自 Cargo 元数据（编译期常量）与 build.rs 注入的构建时间戳。

use super::*;

/// 仓库地址（Cargo.toml 的 repository 字段）
const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

impl App {
    // 打开关于窗口
    pub(super) fn on_open_about(&mut self) {
        self.open_about_window();
    }

    // 关闭关于窗口
    pub(super) fn on_close_about(&self) {
        if let Some(w) = self.about_window.as_ref() {
            w.hide().ok();
        }
    }

    // 打开仓库地址（调用系统默认浏览器）
    pub(super) fn on_open_about_repo(&self) {
        open_url(REPO_URL);
    }

    // 打开关于窗口（懒创建：首次打开时创建，后续仅重新显示并激活）
    fn open_about_window(&mut self) {
        if let Some(w) = self.about_window.as_ref() {
            w.show().ok();
            return;
        }

        let window = AboutWindow::new().expect("failed to create AboutWindow");
        window.set_info(AboutInfo {
            name: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
            description: env!("CARGO_PKG_DESCRIPTION").into(),
            repo: REPO_URL.into(),
            author: env!("CARGO_PKG_AUTHORS").replace(':', ", ").into(),
            license: env!("CARGO_PKG_LICENSE").into(),
            build_time: env!("BUILD_TIMESTAMP").into(),
        });

        // 关闭按钮 → 隐藏窗口
        let command_tx = self.event_tx.clone();
        window.on_close_requested(move || {
            command_tx.send(WindowEvent::CloseAbout).unwrap();
        });

        // 仓库地址点击 → 系统浏览器打开
        let command_tx = self.event_tx.clone();
        window.on_open_repo(move || {
            command_tx.send(WindowEvent::OpenAboutRepo).unwrap();
        });

        // 系统关闭请求 → 隐藏而非销毁
        window
            .window()
            .on_close_requested(move || slint::CloseRequestResponse::HideWindow);

        window.show().expect("failed to show about window");
        self.about_window = Some(window);
    }
}

/// 调用系统默认浏览器打开 URL（跨平台）。失败时仅打印日志，不影响 UI。
fn open_url(url: &str) {
    let result = match std::env::consts::OS {
        "macos" => std::process::Command::new("open").arg(url).status(),
        "windows" => std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status(),
        _ => std::process::Command::new("xdg-open").arg(url).status(),
    };
    if let Err(e) = result {
        eprintln!("[terma] failed to open url {url}: {e}");
    }
}
