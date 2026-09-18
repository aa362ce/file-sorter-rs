mod cli;
mod dedupe;
mod folders;
mod formatting;
mod progress;
mod store;

fn main() {
    match cli::run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("error: {:#}", err);
            std::process::exit(1);
        }
    }
}
