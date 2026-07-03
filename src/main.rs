fn main() {
    if let Err(error) = trelane::run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
