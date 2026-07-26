/// Take `m`, treating a poisoned lock as data rather than as a verdict.
///
/// Everything behind these mutexes is either overwritten whole or is a cache,
/// so a panic in one worker must not turn the daemon into a process that is
/// alive on the bus and answers every call with a panic — systemd will not
/// restart what has not exited.
pub fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
