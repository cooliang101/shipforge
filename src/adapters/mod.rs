//! Operating-system and protocol adapters.

mod paths;
mod process;

pub use paths::{PlatformPathError, user_config_directory, user_home_directory};
pub use process::{ProcessError, ProcessOutput, ProcessTermination, run_grouped};
