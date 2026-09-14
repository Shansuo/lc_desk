//! LC-Deck —— 局域网远控（类 ToDesk，仅限本地局域网）。
//! 同一份二进制既是被控端（服务）也是主控端（查看器）。

mod capture;
mod client;
mod clipboard_sync;
mod config;
mod discovery;
mod input_exec;
mod keys;
mod platform;
mod protocol;
mod server;
mod state;
mod ui;
mod ui_remote;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!(
        "LC-Deck v{} 启动（{}）",
        platform::app_version(),
        platform::platform_name()
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("LC-Deck · 局域网远控")
            .with_inner_size([470.0, 720.0])
            .with_min_inner_size([400.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "LC-Deck",
        options,
        Box::new(|cc| Ok(Box::new(ui::App::new(cc)))),
    )
}
