//! Go `internal/execute/build` (`tsc --build`), with watch mode in orchestrator_watch.rs.

pub mod build_task;
pub mod builders;
pub mod command_line;
pub mod config_prefetch;
pub mod host;
pub mod orchestrator;
pub mod orchestrator_watch;
pub mod parse_cache;
pub mod shared_outputs;
pub mod up_to_date_status;

pub use build_task::*;
pub use command_line::*;
pub use up_to_date_status::*;
