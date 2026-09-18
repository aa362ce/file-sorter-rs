fn main() {
    match file_sorter::cli::run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("error: {:#}", err);
            std::process::exit(1);
        }
    }
}
