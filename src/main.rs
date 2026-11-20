fn main() {
    if let Err(e) = hotbar::cli::run() {
        eprintln!("hotbar: {e:#}");
        std::process::exit(1);
    }
}
