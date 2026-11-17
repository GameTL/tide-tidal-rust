//! The log on disk, for problems noticed after the fact: the console is gone
//! when TIDE is opened from Finder, and the debug panel only keeps a tail.
//!
//! `~/Library/Logs/TIDE/tide.log` on macOS, with the previous file kept beside
//! it as `tide.log.1` once the current one reaches [`MAX_BYTES`].

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let dir = dirs::home_dir()?.join("Library/Logs/TIDE");
    #[cfg(not(target_os = "macos"))]
    let dir = dirs::data_dir()?.join("tidal-rust").join("logs");
    Some(dir.join("tide.log"))
}

pub struct LogFile {
    path: PathBuf,
    file: File,
    written: u64,
}

impl LogFile {
    /// `None` when the file cannot be created; TIDE then logs to the console only.
    pub fn open() -> Option<Mutex<Self>> {
        let path = path()?;
        std::fs::create_dir_all(path.parent()?).ok()?;
        let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Mutex::new(Self {
            path,
            file,
            written,
        }))
    }

    fn rotate(&mut self) -> io::Result<()> {
        let mut previous = self.path.clone().into_os_string();
        previous.push(".1");
        std::fs::rename(&self.path, previous)?;
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= MAX_BYTES {
            // A failed rotation keeps appending to the old file.
            let _ = self.rotate();
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
