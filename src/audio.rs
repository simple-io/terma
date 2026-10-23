//! audio — 终端响铃（BEL \x07）音频播放
//!
//! `Bell` 可跨线程克隆的轻量句柄：内部持有一个「是否启用」的原子开关，
//! 以及一条通往专用音频线程的消息通道。专用音频线程独占 rodio 输出流，收到
//! 触发消息后即时合成一段正弦波短音播放，避免每次响铃重建音频设备的开销。
//!
//! 线程模型：
//!   PTY Output Thread ──(检测到 BEL 且启用)──> bell.ring() ──(mpsc)──> bell-audio 线程 ──> rodio 播放

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::Arc;
use std::thread;

use rodio::OutputStream;
use rodio::Source;
use rodio::buffer::SamplesBuffer;

/// 响铃播放句柄，可跨线程克隆。
#[derive(Clone)]
pub struct Bell {
    // 是否启用响铃：由设置在 UI 线程更新，PTY 解析线程读取。
    enabled: Arc<AtomicBool>,
    // 触发一次响铃的消息发送端。
    trigger: Sender<()>,
}

impl Default for Bell {
    fn default() -> Self {
        Self::new()
    }
}

impl Bell {
    /// 创建响铃播放器并启动专用音频线程。
    pub fn new() -> Self {
        let enabled = Arc::new(AtomicBool::new(false));
        let (trigger, rx) = channel::<()>();

        let _ = thread::Builder::new()
            .name("bell-audio".to_string())
            .spawn(move || {
                // 打开默认音频输出设备；失败（无音频设备等）时静默退出，不影响终端。
                let Ok((_stream, handle)) = OutputStream::try_default() else {
                    return;
                };
                // 通道未关闭期间持续等待触发；_stream / handle 必须保活，否则音频设备被释放。
                while rx.recv().is_ok() {
                    let _ = handle.play_raw(beep_source());
                }
            });

        Self { enabled, trigger }
    }

    /// 设置是否启用响铃（设置在 UI 线程调用）。
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// 触发一次响铃：仅在启用时向音频线程发送播放消息（非阻塞）。
    pub fn ring(&self) {
        if self.enabled.load(Ordering::Relaxed) {
            let _ = self.trigger.send(());
        }
    }
}

/// 合成一段短促的响铃音：880Hz 正弦波，约 150ms，两端淡入淡出避免爆音。
fn beep_source() -> impl Source<Item = f32> + Send + 'static {
    const SAMPLE_RATE: u32 = 44_100;
    const DURATION: f32 = 0.15;
    const FREQ: f32 = 880.0;
    const VOLUME: f32 = 0.3;

    let len = (SAMPLE_RATE as f32 * DURATION) as usize;
    // 10ms 淡入淡出窗口
    let fade = ((SAMPLE_RATE as f32 * 0.01) as usize).max(1);
    let samples: Vec<f32> = (0..len)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            // 包络：开头淡入、结尾淡出，中间保持满幅
            let env = if i < fade {
                i as f32 / fade as f32
            } else if i + fade > len {
                (len - i) as f32 / fade as f32
            } else {
                1.0
            };
            (2.0 * std::f32::consts::PI * FREQ * t).sin() * VOLUME * env
        })
        .collect();

    SamplesBuffer::new(1, SAMPLE_RATE, samples)
}
