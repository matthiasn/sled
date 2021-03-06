use std::cell::UnsafeCell;
use std::fmt::{self, Debug};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

use super::*;

mod lss;
mod iobuf;
mod reservation;
mod periodic_flusher;

pub use self::lss::*;
pub use self::iobuf::*;
pub use self::reservation::*;

/// A trait for objects which facilitate log-structured storage.
pub trait Log: Sized {
    /// Create a log offset reservation for a particular write,
    /// which may later be filled or canceled.
    fn reserve(&self, Vec<u8>) -> Reservation;

    /// Write a buffer to underlying storage.
    fn write(&self, Vec<u8>) -> LogID;

    /// Read a buffer from underlying storage.
    fn read(&self, id: LogID) -> io::Result<LogRead>;

    /// Return the current stable offset.
    fn stable_offset(&self) -> LogID;

    /// Try to flush all pending writes up until the
    /// specified log offset.
    fn make_stable(&self, id: LogID);

    /// Mark the provided message as deletable by the
    /// underlying storage.
    fn punch_hole(&self, id: LogID);

    /// Return the configuration in use by the system.
    fn config(&self) -> &Config;

    /// Return an iterator over the log, starting with
    /// a specified offset.
    fn iter_from(&self, id: LogID) -> LogIter<Self> {
        LogIter {
            next_offset: id,
            log: self,
        }
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub enum LogRead {
    Flush(Vec<u8>, usize),
    Zeroed(usize),
    Corrupted(usize),
}

impl LogRead {
    /// Optionally return successfully read bytes, or None if
    /// the data was corrupt or this log entry was aborted.
    pub fn flush(&self) -> Option<Vec<u8>> {
        match *self {
            LogRead::Flush(ref bytes, _) => Some(bytes.clone()),
            _ => None,
        }
    }

    /// Return true if we read a completed write successfully.
    pub fn is_flush(&self) -> bool {
        match *self {
            LogRead::Flush(_, _) => true,
            _ => false,
        }
    }

    /// Return true if we read an aborted flush.
    pub fn is_zeroed(&self) -> bool {
        match *self {
            LogRead::Zeroed(_) => true,
            _ => false,
        }
    }

    /// Return true if we read a corrupted log entry.
    pub fn is_corrupt(&self) -> bool {
        match *self {
            LogRead::Corrupted(_) => true,
            _ => false,
        }
    }

    /// Retrieve the read bytes from a completed, successful write.
    ///
    /// # Panics
    ///
    /// panics if `is_flush()` is false.
    pub fn unwrap(self) -> Vec<u8> {
        match self {
            LogRead::Flush(bytes, _) => bytes,
            _ => panic!("called unwrap on a non-flush LogRead"),
        }
    }
}

pub struct LogIter<'a, L: 'a + Log> {
    next_offset: LogID,
    log: &'a L,
}

impl<'a, L> Iterator for LogIter<'a, L>
    where L: 'a + Log
{
    type Item = (LogID, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.log.read(self.next_offset) {
                Ok(LogRead::Flush(buf, len)) => {
                    let offset = self.next_offset;
                    self.next_offset += len as LogID + HEADER_LEN as LogID;
                    return Some((offset, buf));
                }
                Ok(LogRead::Zeroed(len)) => {
                    self.next_offset += len as LogID;
                }
                _ => return None,
            }
        }
    }
}

pub fn punch_hole(f: &mut File, id: LogID) -> io::Result<()> {
    f.seek(SeekFrom::Start(id + 1))?;
    let mut len_buf = [0u8; 4];
    f.read_exact(&mut len_buf)?;

    let len32: u32 = unsafe { std::mem::transmute(len_buf) };
    let len = len32 as usize;

    #[cfg(not(target_os = "linux"))]
    {
        use std::io::Write;

        f.seek(SeekFrom::Start(id))?;
        let zeros = vec![0; HEADER_LEN + len];
        f.write_all(&*zeros)?;
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        use libc::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, fallocate};

        let mode = FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE;

        let fd = f.as_raw_fd();

        unsafe {
            fallocate(fd, mode, id as i64, len as i64 + HEADER_LEN as i64);
        }
    }

    Ok(())
}
