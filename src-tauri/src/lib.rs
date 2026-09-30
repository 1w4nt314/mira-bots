pub mod agent;
pub mod config;
pub mod events;
pub mod hooks;
pub mod permissions;
pub mod pipe;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .run(tauri::generate_context!())
        .expect("error while running mira-bots");
}
