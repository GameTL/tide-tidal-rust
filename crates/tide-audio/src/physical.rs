//! Physical format of the output device: the bit depth the samples have on
//! the wire. The system mixer hands every app 32-bit float; the device's
//! physical format decides how many of those bits reach the DAC, so a 24-bit
//! track on a device left at 16-bit is not bit-perfect whatever the rate.

/// Depth to run the device at when `want` bits are asked for: the smallest
/// it offers that holds them, else the deepest it has.
pub fn pick(want: u32, depths: &[u32]) -> Option<u32> {
    depths
        .iter()
        .copied()
        .filter(|bits| *bits >= want)
        .min()
        .or_else(|| depths.iter().copied().max())
}

/// Bit depth of the default output device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Physical {
    /// Depth it runs at now.
    pub bits: u32,
    /// Depths it offers at the asked-for rate, ascending.
    pub depths: Vec<u32>,
}

#[cfg(not(target_os = "macos"))]
pub fn read(_rate: u32) -> Option<Physical> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn set(_rate: u32, _bits: u32) -> bool {
    false
}

#[cfg(target_os = "macos")]
pub use macos::{read, set};

#[cfg(target_os = "macos")]
mod macos {
    use super::Physical;
    use std::ffi::c_void;
    use std::time::Duration;

    #[repr(C)]
    struct PropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    /// `AudioStreamBasicDescription`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Format {
        sample_rate: f64,
        format_id: u32,
        flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels: u32,
        bits: u32,
        reserved: u32,
    }

    /// `AudioStreamRangedDescription`: a format and the rates it comes at.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct RangedFormat {
        format: Format,
        min_rate: f64,
        max_rate: f64,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: u32,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            data_size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            data_size: *mut u32,
            data: *mut c_void,
        ) -> i32;
        fn AudioObjectSetPropertyData(
            object: u32,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            data_size: u32,
            data: *const c_void,
        ) -> i32;
    }

    const fn fourcc(code: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*code)
    }
    const SYSTEM_OBJECT: u32 = 1;
    const GLOBAL: u32 = fourcc(b"glob");
    const OUTPUT: u32 = fourcc(b"outp");
    const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
    const STREAMS: u32 = fourcc(b"stm#");
    const PHYSICAL_FORMAT: u32 = fourcc(b"pft ");
    const PHYSICAL_FORMATS: u32 = fourcc(b"pfta");
    const LINEAR_PCM: u32 = fourcc(b"lpcm");
    /// Formats only an app holding the device exclusively can use.
    const NON_MIXABLE: u32 = 1 << 6;

    /// Every `T` the property holds. `T` must be plain data CoreAudio
    /// documents for this selector.
    fn read_all<T: Copy + Default>(object: u32, selector: u32, scope: u32) -> Option<Vec<T>> {
        let address = PropertyAddress {
            selector,
            scope,
            element: 0,
        };
        let mut size = 0u32;
        // SAFETY: `size` is a valid out-pointer and there is no qualifier.
        let status = unsafe {
            AudioObjectGetPropertyDataSize(object, &address, 0, std::ptr::null(), &mut size)
        };
        if status != 0 {
            return None;
        }
        let mut items = vec![T::default(); size as usize / size_of::<T>()];
        let mut size = (items.len() * size_of::<T>()) as u32;
        // SAFETY: `items` has room for `size` bytes, and CoreAudio writes at
        // most that many, reporting how many it did.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                items.as_mut_ptr().cast(),
            )
        };
        items.truncate(size as usize / size_of::<T>());
        (status == 0).then_some(items)
    }

    /// The default output's first stream, its format, and the formats it
    /// could run at `rate` instead.
    // ponytail: only the first output stream is looked at; a device that
    // splits its channels over several streams needs each one set.
    fn stream(rate: u32) -> Option<(u32, Format, Vec<Format>)> {
        let device = *read_all::<u32>(SYSTEM_OBJECT, DEFAULT_OUTPUT, GLOBAL)?.first()?;
        let stream = *read_all::<u32>(device, STREAMS, OUTPUT)?.first()?;
        let current = *read_all::<Format>(stream, PHYSICAL_FORMAT, GLOBAL)?.first()?;
        let rate = f64::from(rate);
        let offered = read_all::<RangedFormat>(stream, PHYSICAL_FORMATS, GLOBAL)?
            .into_iter()
            .filter(|f| {
                f.format.format_id == LINEAR_PCM
                    && f.format.channels == current.channels
                    && f.format.flags & NON_MIXABLE == current.flags & NON_MIXABLE
                    && (f.format.sample_rate == rate || (f.min_rate..=f.max_rate).contains(&rate))
            })
            .map(|f| Format {
                sample_rate: rate,
                ..f.format
            })
            .collect();
        Some((stream, current, offered))
    }

    pub fn read(rate: u32) -> Option<Physical> {
        let (_, current, offered) = stream(rate)?;
        let mut depths: Vec<u32> = offered.iter().map(|f| f.bits).collect();
        depths.sort_unstable();
        depths.dedup();
        Some(Physical {
            bits: current.bits,
            depths,
        })
    }

    /// Run the default output at `bits` and `rate`. The device switches a
    /// moment after it is told to: this waits (up to a second) until it has.
    pub fn set(rate: u32, bits: u32) -> bool {
        let Some((stream, _, offered)) = stream(rate) else {
            return false;
        };
        let Some(format) = offered.iter().find(|f| f.bits == bits) else {
            return false;
        };
        let address = PropertyAddress {
            selector: PHYSICAL_FORMAT,
            scope: GLOBAL,
            element: 0,
        };
        // SAFETY: the property takes one `AudioStreamBasicDescription`, which
        // `Format` mirrors field for field.
        let status = unsafe {
            AudioObjectSetPropertyData(
                stream,
                &address,
                0,
                std::ptr::null(),
                size_of::<Format>() as u32,
                (format as *const Format).cast(),
            )
        };
        if status != 0 {
            return false;
        }
        for _ in 0..50 {
            if read(rate).is_some_and(|now| now.bits == bits) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::pick;

    #[test]
    fn picks_the_smallest_depth_that_holds_the_track() {
        assert_eq!(pick(24, &[16, 24]), Some(24));
        assert_eq!(pick(16, &[16, 24]), Some(16));
        assert_eq!(pick(20, &[16, 24, 32]), Some(24));
        // Nothing deep enough: the deepest there is.
        assert_eq!(pick(32, &[16, 24]), Some(24));
        assert_eq!(pick(u32::MAX, &[16, 24, 32]), Some(32));
        assert_eq!(pick(24, &[]), None);
    }
}
