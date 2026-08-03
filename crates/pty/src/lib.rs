//! PTY proxy event loop.
//!
//! Spawns the user's shell via `portable-pty`, multiplexes stdin/stdout with
//! `tokio::select!`, handles `SIGWINCH` resize, and intercepts keystrokes
//! for popup navigation.

mod config_watch;
pub mod dynamic_result;
pub mod feedback;
pub mod handler;
pub mod input;
pub mod predict;
mod proxy;
mod resize;
pub mod shell_completion;
mod spawn;

pub use handler::parse_key_name;
pub use overlay::parse_style;
pub use proxy::run_proxy;
