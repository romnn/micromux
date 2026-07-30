#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("failed to create a kill-on-close job object")]
    Create(#[source] win32job::JobError),
    #[error("failed to assign the process to its job object")]
    Assign(#[source] win32job::JobError),
}

/// Attach a process to a kill-on-close job so dropping the job terminates remaining descendants.
///
/// # Errors
///
/// Returns an error when the job cannot be created or the process cannot be assigned to it.
pub(crate) fn attach_kill_on_close(process: isize) -> Result<win32job::Job, Error> {
    let mut limits = win32job::ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&limits).map_err(Error::Create)?;
    job.assign_process(process).map_err(Error::Assign)?;
    Ok(job)
}
