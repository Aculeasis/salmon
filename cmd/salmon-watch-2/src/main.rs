fn main() {
    if let Err(error) = salmon_watch::app::execute() {
        if salmon_watch::logging::is_initialized() {
            log::error!("{error:#}");
        } else {
            eprintln!("salmon-watch: {error:#}");
        }
        std::process::exit(1);
    }
}
