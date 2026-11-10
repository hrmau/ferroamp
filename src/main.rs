use std::path::PathBuf;

use ferroamp::ui::{App, UserEvent};
use winit::event_loop::EventLoop;

fn main() {
    let args: Vec<PathBuf> = std::env::args_os()
        .skip(1)
        // Launch Services adds this when starting an app from Finder.
        .filter(|a| !a.to_string_lossy().starts_with("-psn_"))
        .map(PathBuf::from)
        .collect();

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            eprintln!("ferroamp: cannot start: {e}");
            std::process::exit(1);
        }
    };
    ferroamp::macos::install_open_handler(event_loop.create_proxy());
    ferroamp::macos::install_remote_commands(event_loop.create_proxy());

    let mut app = App::new(event_loop.create_proxy(), args);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("ferroamp: {e}");
        std::process::exit(1);
    }
}
