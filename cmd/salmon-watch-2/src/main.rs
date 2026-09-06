fn main() {
    if let Err(error) = salmon_watch_2::app::execute() {
        eprintln!("salmon-watch-2: {error:#}");
        std::process::exit(1);
    }
}
