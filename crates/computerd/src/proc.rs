//! Signals to other processes. With `shm`, the only module that needs `unsafe`.

/// Sends SIGKILL to every process in the group whose leader has id `group`.
pub fn kill_group(group: u32) {
    if let Ok(group) = i32::try_from(group) {
        // SAFETY: killpg takes plain integers and has no memory effects. At worst the group is gone.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
}

/// Asks the process to quit with SIGTERM.
pub fn terminate(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: kill takes plain integers and has no memory effects. At worst the process is gone.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}
