pub mod command;
pub mod kiss;
pub mod response;
pub mod server;
pub mod writer;

use std::fmt;
use std::sync::OnceLock;

static LOG: OnceLock<fn(fmt::Arguments)> = OnceLock::new();

pub fn set_log(f: fn(fmt::Arguments)) {
    let _ = LOG.set(f);
}

fn say(args: fmt::Arguments) {
    match LOG.get() {
        Some(f) => f(args),
        None => eprintln!("xara: {args}"),
    }
}
