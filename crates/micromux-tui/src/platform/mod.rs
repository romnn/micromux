//! What the TUI does differently per platform, behind one interface the rest of the crate calls
//! everywhere.
//!
//! Unix has job control, terminal device names and process groups.
//! Other platforms have none of these, so their side does nothing, never fires or reports that
//! the operation is unsupported.

#[cfg_attr(unix, path = "unix.rs")]
#[cfg_attr(not(unix), path = "other.rs")]
mod imp;

pub(crate) use imp::{Continued, detach, own_tty, stop_process_group};
