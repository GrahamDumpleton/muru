//! The built-in init for a container (design specification section 6.10):
//! when muructl is PID 1, it forks before anything else, tokio included.
//! The child runs the command; the parent stays as PID 1 and does what
//! tini does: forwards the stop and reopen signals to the child, collects
//! every process that exits, so orphans reparented to it never sit as
//! zombies, and exits with the child's status when the child exits.
//! `MURU_NO_INIT=1` opts out, for images that bring an init of their own.
//! A `muructl daemon upgrade` re-executes the child, so PID 1 never
//! changes.

/// Opts out of the built-in init when set to 1.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const NO_INIT: &str = "MURU_NO_INIT";

/// The signals the init passes on to the child; everything else keeps its
/// default meaning for PID 1, which is to be ignored.
#[cfg(target_os = "linux")]
const FORWARDED: [libc::c_int; 6] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGQUIT,
];

/// Fork if this process is a container's PID 1 and nothing opted out:
/// the child returns to run the command, the parent supervises it and
/// never returns. Everywhere else, and when the fork fails, this returns
/// at once and the command runs as PID 1 itself.
#[cfg(target_os = "linux")]
pub fn become_init() {
    if std::process::id() != 1 || std::env::var_os(NO_INIT).is_some_and(|value| value == "1") {
        return;
    }

    // The forwarded signals and SIGCHLD are blocked before the fork, so
    // one arriving before the parent waits for them is kept pending
    // rather than lost, and sigwaitinfo below is how they are taken. The
    // child restores the mask it would have had.

    // SAFETY: a signal set is plain data for which all zeroes is a valid
    // value, and sigemptyset fills it in below.

    let mut waited: libc::sigset_t = unsafe { std::mem::zeroed() };

    // SAFETY: as above; sigprocmask writes the current mask into it.

    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };

    // SAFETY: the sigset functions write only to the sets passed, which
    // are owned here, and sigprocmask is called in a single-threaded
    // process before anything else starts.

    unsafe {
        libc::sigemptyset(&mut waited);

        for signal in FORWARDED {
            libc::sigaddset(&mut waited, signal);
        }

        libc::sigaddset(&mut waited, libc::SIGCHLD);
        libc::sigprocmask(libc::SIG_BLOCK, &waited, &mut previous);
    }

    // SAFETY: fork(2) in a single-threaded process at the start of main;
    // neither process touches anything the other holds.

    let child = unsafe { libc::fork() };

    match child {
        -1 => {
            let error = std::io::Error::last_os_error();

            // SAFETY: as above; the mask goes back to what it was.

            unsafe {
                libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
            }

            eprintln!("muru: could not start as init ({error}); running as PID 1 without one");
        }
        0 => {
            // SAFETY: as above, in the child, which is single-threaded;
            // set_var is sound while no other thread can read the
            // environment, which holds until main goes on.

            unsafe {
                libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
                std::env::set_var(muru_controller::shutdown::UNDER_INIT, "1");
            }
        }
        child => supervise(child, &waited),
    }
}

/// The parent's life as PID 1: take each signal as it comes, pass the
/// forwarded ones to the child, collect whatever exited on SIGCHLD, and
/// leave with the child's status once the child is among them.
#[cfg(target_os = "linux")]
fn supervise(child: libc::pid_t, waited: &libc::sigset_t) -> ! {
    loop {
        // SAFETY: siginfo_t is plain data for which all zeroes is a valid
        // value; sigwaitinfo fills it in.

        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };

        // SAFETY: the set is the one blocked above and info is owned here;
        // sigwaitinfo blocks until one of the set is pending.

        let signal = unsafe { libc::sigwaitinfo(waited, &mut info) };

        if signal == -1 {
            continue;
        }

        if signal != libc::SIGCHLD {
            // SAFETY: kill(2) with the child's pid and a signal from the
            // forwarded set has no memory-safety preconditions.

            unsafe {
                libc::kill(child, signal);
            }

            continue;
        }

        // Every exited process is collected, not only the child: an
        // orphan reparented to PID 1 would otherwise stay a zombie. One
        // SIGCHLD may stand for several exits.

        loop {
            let mut status: libc::c_int = 0;

            // SAFETY: waitpid(2) writes the status to the integer passed,
            // which is owned here.

            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };

            if pid <= 0 {
                break;
            }

            if pid == child {
                std::process::exit(exit_code(status));
            }
        }
    }
}

/// The exit code for a wait status: the child's own, or 128 plus the
/// signal that killed it, as a shell reports it.
#[cfg(target_os = "linux")]
fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

/// Only Linux containers make muructl PID 1.
#[cfg(not(target_os = "linux"))]
pub fn become_init() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::exit_code;

    #[test]
    fn exit_codes_follow_the_shell() {
        // A status word: the exit code in the high byte, the signal in
        // the low byte.

        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(libc::SIGTERM), 128 + libc::SIGTERM);
        assert_eq!(exit_code(libc::SIGKILL), 137);
    }
}
