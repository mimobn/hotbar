// Same as `hotbar`, but on Windows it opens no console window.
// Autostart uses `hotbarw daemon`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if let Err(e) = hotbar::cli::run() {
        hotbar::log!("hotbar: {e:#}");
        std::process::exit(1);
    }
}
