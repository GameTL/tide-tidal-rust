//! Process memory readings for the debug panel's RAM inspector.

/// Physical footprint of this process in bytes: the number Activity Monitor
/// shows as "Memory" (dirty + compressed, IOSurfaces and GPU memory included).
#[cfg(target_os = "macos")]
pub fn footprint() -> u64 {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: the buffer is a rusage_info_v4, which is what flavour V4 writes.
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr() as *mut libc::rusage_info_t,
        )
    };
    if rc != 0 {
        return 0;
    }
    // SAFETY: proc_pid_rusage succeeded and filled the struct.
    let info = unsafe { info.assume_init() };
    info.ri_phys_footprint
}

#[cfg(not(target_os = "macos"))]
pub fn footprint() -> u64 {
    0
}

/// Bytes handed out by malloc across every zone (Rust's heap included).
#[cfg(target_os = "macos")]
pub fn malloc_in_use() -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct MallocStatistics {
        blocks_in_use: libc::c_uint,
        size_in_use: libc::size_t,
        max_size_in_use: libc::size_t,
        size_allocated: libc::size_t,
    }
    unsafe extern "C" {
        fn malloc_zone_statistics(zone: *mut libc::c_void, stats: *mut MallocStatistics);
    }
    let mut stats = MallocStatistics::default();
    // SAFETY: a null zone asks for the sum over all zones; `stats` is valid.
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
    stats.size_in_use as u64
}

#[cfg(not(target_os = "macos"))]
pub fn malloc_in_use() -> u64 {
    0
}

/// Ask every malloc zone to hand its free pages back to the system, so the
/// footprint drops after the window's buffers are released. Returns the bytes
/// returned.
#[cfg(target_os = "macos")]
#[allow(dead_code)] // The renderer_memory example shares this file.
pub fn release_free_memory() -> u64 {
    unsafe extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: libc::size_t)
        -> libc::size_t;
    }
    // SAFETY: a null zone means all zones; a goal of 0 means as much as possible.
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) as u64 }
}

#[cfg(not(target_os = "macos"))]
pub fn release_free_memory() -> u64 {
    0
}

/// One reading, split the way the inspector draws it. All sizes in bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Breakdown {
    pub artwork: u64,
    pub snapshots: u64,
    pub heap: u64,
    pub window: u64,
    pub other: u64,
}

impl Breakdown {
    /// Split a footprint into parts. `artwork` and `snapshots` are pixel
    /// buffers counted by the app and live inside malloc; the window buffers
    /// are an estimate and live outside it. Each part is clamped so the parts
    /// never add up to more than the footprint.
    pub fn split(footprint: u64, malloc: u64, artwork: u64, snapshots: u64, window: u64) -> Self {
        let malloc = malloc.min(footprint);
        let artwork = artwork.min(malloc);
        let snapshots = snapshots.min(malloc - artwork);
        let heap = malloc - artwork - snapshots;
        let window = window.min(footprint - malloc);
        let other = footprint - malloc - window;
        Self {
            artwork,
            snapshots,
            heap,
            window,
            other,
        }
    }

    pub fn total(&self) -> u64 {
        self.artwork + self.snapshots + self.heap + self.window + self.other
    }
}

/// "850 MB", "1.2 GB", "412 KB".
pub fn human(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= 1024.0 * MB {
        format!("{:.2} GB", b / (1024.0 * MB))
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_adds_up_to_footprint() {
        let b = Breakdown::split(1000, 600, 300, 100, 150);
        assert_eq!(
            b,
            Breakdown {
                artwork: 300,
                snapshots: 100,
                heap: 200,
                window: 150,
                other: 250
            }
        );
        assert_eq!(b.total(), 1000);
    }

    #[test]
    fn split_clamps_overcounts() {
        let b = Breakdown::split(500, 800, 900, 50, 400);
        assert_eq!(b.total(), 500);
        assert_eq!(b.artwork, 500);
        assert_eq!(b.window, 0);
    }

    #[test]
    fn readings_are_nonzero_on_macos() {
        if cfg!(target_os = "macos") {
            assert!(footprint() > 0);
            assert!(malloc_in_use() > 0);
        }
    }

    #[test]
    fn human_units() {
        assert_eq!(human(850 * 1024 * 1024), "850 MB");
        assert_eq!(human(2048), "2 KB");
    }
}
