#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--headless") {
        if let Err(error) = seamless_image_edit_lib::run_headless_cli(args) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    seamless_image_edit_lib::run()
}
