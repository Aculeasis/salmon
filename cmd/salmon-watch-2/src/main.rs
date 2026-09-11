fn main() {
    if let Err(error) = salmon_watch_2::app::execute() {
        if salmon_watch_2::logging::is_initialized() {
            log::error!("{error:#}");
        } else {
            eprintln!("salmon-watch-2: {error:#}");
        }
        std::process::exit(1);
    }
}
