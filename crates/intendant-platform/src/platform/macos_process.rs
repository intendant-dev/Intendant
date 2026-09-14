//! Process identity probe in the existing platform unsafe island.

/// A PID alone is not an identity: reject reuse after an application restarts.
pub fn macos_process_birth(pid: i32) -> Option<(u64, u64)> {
    if pid <= 0 { return None; }
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: kernel writes at most size bytes into aligned storage. No fields
    // are read unless the complete documented C struct has been returned.
    let written = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size as i32)
    };
    if written != size as i32 { return None; }
    // SAFETY: exact-sized successful read initialized the complete C struct.
    let info = unsafe { info.assume_init() };
    Some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_processes_are_rejected_without_a_syscall() {
        assert_eq!(macos_process_birth(0), None);
        assert_eq!(macos_process_birth(-1), None);
    }
    #[test]
    fn current_test_process_has_a_stable_generation() {
        let pid = std::process::id() as i32;
        let first = macos_process_birth(pid).expect("test process must exist");
        assert!(first.0 > 0);
        assert_eq!(macos_process_birth(pid), Some(first));
    }
}
