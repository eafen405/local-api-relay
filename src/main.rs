mod auth;
mod backup;
mod cli;
mod log;
mod paths;
mod server;
mod store;
mod timeutil;
mod web;

use clap::Parser;

#[tokio::main]
async fn main() {
    // All subsequently created local state is private to the current OS user.
    // `umask` is Unix-only; Windows user profile directories already provide
    // the per-user boundary for the AppData locations used below.
    #[cfg(unix)]
    unsafe {
        libc::umask(0o077)
    };

    if let Err(error) = cli::run(cli::Cli::parse()).await {
        eprintln!("local-api-relay: {error:#}");
        std::process::exit(1);
    }
}
