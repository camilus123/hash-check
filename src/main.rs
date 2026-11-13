// No console window behind the GUI in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    hash_check::ui::run();
}
