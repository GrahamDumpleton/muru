//! The worker's own memory, for the host's memory policy (design
//! specification section 7.7): resident, proportional and unique set
//! sizes, from the kernel's accounting on Linux and the task's footprint
//! on macOS.

/// A worker's memory in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Memory {
    /// Resident set size: every resident page.
    pub rss: u64,
    /// Proportional set size: shared pages counted in proportion.
    pub pss: u64,
    /// Unique set size: pages no other process shares.
    pub uss: u64,
}

/// Measure this process, or none where the platform offers no figures.
#[cfg(target_os = "linux")]
pub fn measure() -> Option<Memory> {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    let mut memory = Memory::default();
    let mut private = 0;

    // Each line is a name, a value in kB and the unit.

    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let kib: u64 = value.parse().unwrap_or(0);

        match name {
            "Rss:" => memory.rss = kib * 1024,
            "Pss:" => memory.pss = kib * 1024,
            "Private_Clean:" | "Private_Dirty:" => private += kib * 1024,
            _ => {}
        }
    }

    memory.uss = private;
    Some(memory)
}

/// Measure this process: macOS reports the resident size and the
/// physical footprint, which stands in for the unique and proportional
/// sizes it does not break out.
#[cfg(target_os = "macos")]
pub fn measure() -> Option<Memory> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::uninit();

    // SAFETY: proc_pid_rusage fills a buffer of the size the flavour
    // implies, which this frame owns; the pid is this process's own.

    let result = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr().cast(),
        )
    };

    if result != 0 {
        return None;
    }

    // SAFETY: the call succeeded, so the buffer holds a rusage_info_v2.

    let info = unsafe { info.assume_init() };

    Some(Memory {
        rss: info.ri_resident_size,
        pss: info.ri_phys_footprint,
        uss: info.ri_phys_footprint,
    })
}

/// No figures on other platforms.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn measure() -> Option<Memory> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_some_resident_memory() {
        let memory = measure().expect("a measurement on this platform");
        assert!(memory.rss > 0);
        assert!(memory.uss > 0);
    }
}
