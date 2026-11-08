//! What the TUI does differently per platform, behind one interface the rest of the crate calls
//! everywhere.
//!
//! Unix has job control, terminal device names and process groups.
//! Other platforms have none of these, so their side does nothing, never fires or reports that
//! the operation is unsupported.

cfg_select! {
    unix => {
        mod unix;
        pub(crate) use unix::{Continued, detach, own_tty, stop_process_group};
    }
    _ => {
        mod other;
        pub(crate) use other::{Continued, detach, own_tty, stop_process_group};
    }
}
