//! Process plumbing: parent-death detection (same rules as cu-abort-watch),
//! `poll` and raw reads.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

/// Optional pid of the process the overlay serves (Claude's main process).
pub const PARENT_PID_ENV: &str = "CLAUDE_CU_PARENT_PID";

/// `(state, starttime)` from a `/proc/<pid>/stat` line: fields 3 and 22,
/// counted after the last `)` since comm may contain spaces and parentheses.
pub(crate) fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

/// Start time of a RUNNING process: `None` when it does not exist or is a
/// zombie / dead.
pub(crate) fn proc_start_time(pid: u32) -> Option<u64> {
    let (state, start) = parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    (!matches!(state, 'Z' | 'X')).then_some(start)
}

/// The process whose death ends the overlay.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Parent {
    /// Our parent at startup; gone when `getppid` changes.
    Ppid(libc::pid_t),
    /// `CLAUDE_CU_PARENT_PID`; gone when it no longer runs with the same
    /// start time.
    Pid { pid: u32, start: u64 },
    /// `CLAUDE_CU_PARENT_PID` named a process that was not running.
    AlreadyGone,
}

impl Parent {
    /// Record who to watch and ask the kernel for SIGTERM when the direct
    /// parent dies (kept in both modes). `CLAUDE_CU_PARENT_PID` (> 1) is
    /// watched by pid and start time instead of the `getppid` snapshot.
    pub(crate) fn watch() -> Self {
        // SAFETY: getppid and prctl(PR_SET_PDEATHSIG) have no memory effects.
        let ppid = unsafe { libc::getppid() };
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGTERM as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            );
        }
        let named = std::env::var(PARENT_PID_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|&p| p > 1);
        match named {
            Some(pid) => match proc_start_time(pid) {
                Some(start) => Parent::Pid { pid, start },
                None => Parent::AlreadyGone,
            },
            None => Parent::Ppid(ppid),
        }
    }

    pub(crate) fn gone(&self) -> bool {
        match *self {
            // SAFETY: getppid has no memory effects.
            Parent::Ppid(p) => unsafe { libc::getppid() != p },
            Parent::Pid { pid, start } => proc_start_time(pid) != Some(start),
            Parent::AlreadyGone => true,
        }
    }
}

/// Readiness of the two polled descriptors.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Ready {
    pub(crate) display: bool,
    /// stdin is readable, at EOF, or invalid.
    pub(crate) input: bool,
}

pub(crate) fn poll2(display: RawFd, input: Option<RawFd>, timeout: Option<Duration>) -> Ready {
    let mut fds = [
        libc::pollfd {
            fd: display,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: input.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let ms = timeout.map_or(-1, |t| {
        // round up so a deadline 0.4 ms away does not spin
        (t.as_micros().div_ceil(1000)).min(i32::MAX as u128) as libc::c_int
    });
    // SAFETY: fds is a valid array of two pollfd.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
    if n <= 0 {
        return Ready::default();
    }
    Ready {
        display: fds[0].revents != 0,
        input: fds[1].revents != 0,
    }
}

/// What a raw read returned.
pub(crate) enum ReadResult {
    Data(usize),
    Eof,
    Again,
}

pub(crate) fn read_fd(fd: RawFd, buf: &mut [u8]) -> ReadResult {
    // SAFETY: reading into the caller's buffer of the given length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        return ReadResult::Data(n as usize);
    }
    if n == 0 {
        return ReadResult::Eof;
    }
    match io::Error::last_os_error().kind() {
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => ReadResult::Again,
        _ => ReadResult::Eof,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_odd_comm() {
        let stat = "1234 (a) b (c) S 1 1234 1234 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 987654 0";
        assert_eq!(parse_stat(stat), Some(('S', 987654)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn own_process_is_running() {
        assert!(proc_start_time(std::process::id()).is_some());
    }
}
