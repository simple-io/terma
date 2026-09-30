//! terma — 跨平台 AI 终端

#![allow(dead_code, unused_imports)]

mod app;
mod platform;
mod profile;
mod pty;
mod renderer;
mod session;
mod terminal;

fn main() {
    app::App::run();
}
