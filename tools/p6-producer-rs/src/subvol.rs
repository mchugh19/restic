//! BTRFS_IOC_GET_SUBVOL_INFO: a subvolume's uuid and creation
//! time (the ROOT_ITEM otime), read from any handle inside the
//! subvolume.
//!
//! The ctime proof (see resolve.rs and CTIME_DESIGN.md) needs
//! exactly one fact about the retained snapshot: the wall-clock
//! moment it was created. This ioctl returns that fact, at
//! nanosecond precision, in the same result as the subvolume's
//! uuid, so the timestamp is tied to the snapshot's identity by
//! construction: it is read fresh from the retained root on
//! every run, and no lifecycle path (adoption, re-base, init)
//! can leave a stale value attached to a different snapshot,
//! because the value is never stored.
//!
//! Layout of struct btrfs_ioctl_get_subvol_info_args (kernel
//! UAPI, <linux/btrfs.h>), 504 bytes in all:
//!   treeid @0, name[256] @8, parent_id @264, dirid @272,
//!   generation @280, flags @288, uuid[16] @296,
//!   parent_uuid[16] @312, received_uuid[16] @328,
//!   ctransid @344, otransid @352, stransid @360, rtransid @368,
//!   ctime (sec @376, nsec @384), otime (sec @392, nsec @400),
//!   stime @408, rtime @424, reserved @440.
//! The request number is _IOR(0x94, 60, 504) = 0x81f8943c
//! (direction read, size 504, magic 0x94, nr 60), verified
//! against the C header on the build host and against the
//! kernel on the testbed (kernel 6.17), where a snapshot's
//! otime was confirmed to be the snapshot's own creation time,
//! inside the wall-clock window of its `btrfs subvolume
//! snapshot` command.
//!
//! Any failure is an Err/None to the caller, which treats it as
//! "no threshold" and disables the ctime proof for the run;
//! this module never panics on kernel-supplied data.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

extern "C" {
    fn ioctl(fd: i32, request: u64, arg: *mut c_void) -> i32;
}

const IOC_GET_SUBVOL_INFO: u64 = 0x81f8_943c;
const ARGS_LEN: usize = 504;
const UUID_OFF: usize = 296;
const UUID_LEN: usize = 16;
const OTIME_SEC_OFF: usize = 392;
const OTIME_NSEC_OFF: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubvolInfo {
    /// The subvolume's uuid, in `btrfs subvolume show` spelling
    /// (lowercase hex, hyphenated 8-4-4-4-12).
    pub uuid: String,
    /// The subvolume's creation time (ROOT_ITEM otime) as
    /// (seconds, nanoseconds) since the epoch.
    pub otime: (i64, u32),
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn format_uuid(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Decode one filled result buffer. None when the buffer is
/// short, the uuid is all zero (not a real subvolume), or the
/// otime is not a well-formed timespec: the caller treats every
/// one of those as "no threshold".
fn decode(buf: &[u8]) -> Option<SubvolInfo> {
    if buf.len() < ARGS_LEN {
        return None;
    }
    let uuid_bytes = &buf[UUID_OFF..UUID_OFF + UUID_LEN];
    if uuid_bytes.iter().all(|&b| b == 0) {
        return None;
    }
    let sec = get_u64(buf, OTIME_SEC_OFF);
    let nsec = get_u32(buf, OTIME_NSEC_OFF);
    if nsec >= 1_000_000_000 {
        return None;
    }
    Some(SubvolInfo {
        uuid: format_uuid(uuid_bytes),
        otime: (sec as i64, nsec),
    })
}

/// The uuid and creation time of the subvolume `root` is a
/// handle on (any handle inside the subvolume works; the
/// kernel reports the containing subvolume).
///
/// The success return is rc >= 0, not rc == 0: this ioctl
/// returns 1 on success on the kernels in use (observed on the
/// testbed, kernel 6.17: return 1, errno 0, buffer fully
/// written). Errors are the usual -1 with errno set. The
/// decoded fields are sanity-checked in decode(), and the
/// resolver cross-checks the pair (uuids differ, otimes
/// ordered, retained otime not in the run's future) before
/// trusting a value, so a permissive success test cannot
/// manufacture a threshold out of a failed call.
pub fn subvol_info(root: &File) -> io::Result<SubvolInfo> {
    let mut buf = vec![0u8; ARGS_LEN];
    let rc = unsafe {
        ioctl(
            root.as_raw_fd(),
            IOC_GET_SUBVOL_INFO,
            buf.as_mut_ptr() as *mut c_void,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    decode(&buf).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "subvol info: malformed result from the kernel",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(uuid: &[u8; 16], sec: u64, nsec: u32) -> Vec<u8> {
        let mut buf = vec![0u8; ARGS_LEN];
        buf[UUID_OFF..UUID_OFF + UUID_LEN].copy_from_slice(uuid);
        buf[OTIME_SEC_OFF..OTIME_SEC_OFF + 8]
            .copy_from_slice(&sec.to_le_bytes());
        buf[OTIME_NSEC_OFF..OTIME_NSEC_OFF + 4]
            .copy_from_slice(&nsec.to_le_bytes());
        buf
    }

    #[test]
    fn decodes_uuid_and_otime() {
        let mut uuid = [0u8; 16];
        for (i, b) in uuid.iter_mut().enumerate() {
            *b = i as u8;
        }
        let info = decode(&buffer(&uuid, 1_700_000_000, 123_456_789))
            .expect("well-formed buffer must decode");
        assert_eq!(info.uuid, "00010203-0405-0607-0809-0a0b0c0d0e0f");
        assert_eq!(info.otime, (1_700_000_000, 123_456_789));
    }

    #[test]
    fn zero_uuid_is_no_info() {
        assert!(decode(&buffer(&[0u8; 16], 1_700_000_000, 0)).is_none());
    }

    #[test]
    fn bad_nsec_is_no_info() {
        let mut uuid = [0u8; 16];
        uuid[0] = 1;
        assert!(decode(&buffer(&uuid, 1_700_000_000, 1_000_000_000))
            .is_none());
    }

    #[test]
    fn short_buffer_is_no_info() {
        assert!(decode(&[0u8; 100]).is_none());
    }
}
