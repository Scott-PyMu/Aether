#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Some(code) = aether_tauri::cli_exit_code(std::env::args().skip(1)) {
        std::process::exit(code);
    }
    if let Err(err) = aether_tauri::run() {
        eprintln!("Aether 启动失败：{err}");
        std::process::exit(1);
    }
}
