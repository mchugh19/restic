//! Streaming reader for the btrfs send stream (documented TLV
//! format, stream versions 1 and 2).
//!
//! Wire format, confirmed byte-for-byte against streams produced by
//! btrfs-progs 6.6.3 / kernel 6.17 on the OCI testbed:
//!
//!   header   : 13-byte magic "btrfs-stream\0", then a little-endian
//!              u32 stream version (1 or 2).
//!   command  : u32 payload length, u16 command, u32 crc; then the
//!              payload: a sequence of attributes, each u16 type,
//!              u16 length, value bytes. The crc is CRC-32C in the
//!              kernel crc32c() convention over the command header
//!              with the crc field zeroed, followed by the payload.
//!   sentinel : a command with cmd 21 and length 0 ends the stream.
//!              (In stream v2, cmd 21 is fallocate; a fallocate
//!              command carries a path and a non-empty payload, so
//!              the two are told apart by the payload length.)
//!
//! The reader pulls one command at a time from any `Read` (a pipe
//! from a running `btrfs send`, or a file). Memory stays bounded by
//! the largest single command: the stream as a whole is never
//! buffered, and the payload buffer is reused between commands.
//!
//! Command numbers (kernel send.h, as decoded and cross-checked
//! line-by-line against `btrfs receive --dump` for the same stream):
//!   1 subvol, 2 snapshot, 3 mkfile, 4 mkdir, 5 mknod, 6 mkfifo,
//!   7 mksock, 8 symlink, 9 rename, 10 link, 11 unlink, 12 rmdir,
//!   13 set_xattr, 14 remove_xattr, 15 write, 16 clone, 17 truncate,
//!   18 set_mode (chmod), 19 set_owner (chown), 20 set_times
//!   (utimes), 21 end / fallocate, 22 update_extent.
//!
//! Classification matches sendparse.py, the Phase A reference
//! parser: creates, content changes (write, clone, truncate,
//! fallocate, update_extent) and metadata changes (chmod, chown,
//! utimes, xattrs) are all "modify" for list purposes, except the
//! create commands, which are "create". The distinction does not
//! change the output sets (both land in the changed list) but is
//! kept because the resolver's diagnostics mirror the reference.

use std::fmt;
use std::io::{self, Read};

use crate::crc32c::Crc32c;

pub const STREAM_MAGIC: &[u8; 13] = b"btrfs-stream\0";

// Attribute types used by the resolver.
pub const ATTR_PATH: u16 = 15;
pub const ATTR_PATH_TO: u16 = 16;
pub const ATTR_PATH_LINK: u16 = 17;

// Command numbers.
pub const CMD_SUBVOL: u16 = 1;
pub const CMD_SNAPSHOT: u16 = 2;
pub const CMD_MKFILE: u16 = 3;
pub const CMD_MKDIR: u16 = 4;
pub const CMD_MKNOD: u16 = 5;
pub const CMD_MKFIFO: u16 = 6;
pub const CMD_MKSOCK: u16 = 7;
pub const CMD_SYMLINK: u16 = 8;
pub const CMD_RENAME: u16 = 9;
pub const CMD_LINK: u16 = 10;
pub const CMD_UNLINK: u16 = 11;
pub const CMD_RMDIR: u16 = 12;
pub const CMD_SET_XATTR: u16 = 13;
pub const CMD_REMOVE_XATTR: u16 = 14;
pub const CMD_WRITE: u16 = 15;
pub const CMD_CLONE: u16 = 16;
pub const CMD_TRUNCATE: u16 = 17;
pub const CMD_SET_MODE: u16 = 18;
pub const CMD_SET_OWNER: u16 = 19;
pub const CMD_SET_TIMES: u16 = 20;
pub const CMD_END_OR_FALLOCATE: u16 = 21;
pub const CMD_UPDATE_EXTENT: u16 = 22;

/// Largest command payload accepted. A --no-data stream's commands
/// are metadata-sized (the largest seen in the corpus work is a
/// set_xattr with a large value, well under 1 MiB); even a data-mode
/// write command carries at most one extent buffer. 32 MiB is far
/// above any legitimate command and stops a corrupt length field
/// from turning into an unbounded allocation.
const MAX_CMD_LEN: u32 = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum StreamError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u32),
    /// Stored crc does not match the computed one: the stream is
    /// corrupt (or was truncated into a misleading shape).
    CrcMismatch { cmd: u16, stored: u32, computed: u32 },
    /// The stream ended in the middle of a command, or without the
    /// end-of-stream sentinel.
    Truncated(&'static str),
    CommandTooLarge(u32),
    UnknownCommand(u16),
    MalformedAttrs(u16),
    MissingAttr { cmd: u16, attr: u16 },
    TrailingBytesAfterEnd(usize),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Io(e) => write!(f, "stream read error: {e}"),
            StreamError::BadMagic => write!(f, "not a btrfs send stream (bad magic)"),
            StreamError::UnsupportedVersion(v) => {
                write!(f, "unsupported send stream version {v}")
            }
            StreamError::CrcMismatch {
                cmd,
                stored,
                computed,
            } => write!(
                f,
                "crc mismatch on command {cmd}: stored {stored:08x}, computed {computed:08x} (stream corrupt)"
            ),
            StreamError::Truncated(what) => write!(f, "stream truncated: {what}"),
            StreamError::CommandTooLarge(n) => {
                write!(f, "command payload of {n} bytes exceeds the sanity cap")
            }
            StreamError::UnknownCommand(c) => write!(f, "unknown send command {c}"),
            StreamError::MalformedAttrs(c) => {
                write!(f, "malformed attribute block in command {c}")
            }
            StreamError::MissingAttr { cmd, attr } => {
                write!(f, "command {cmd} lacks required attribute {attr}")
            }
            StreamError::TrailingBytesAfterEnd(n) => {
                write!(f, "{n} trailing bytes after the end-of-stream command")
            }
        }
    }
}

impl std::error::Error for StreamError {}

impl From<io::Error> for StreamError {
    fn from(e: io::Error) -> Self {
        StreamError::Io(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Create,
    Modify,
    Delete,
    Link,
    Rename,
}

/// One classified stream operation, in stream order. Paths are raw
/// bytes, relative to the snapshot root ("" is the root itself),
/// spelled exactly as the kernel emits them: newly created objects
/// appear under staging names (`o<ino>-<transid>-0`) and renamed
/// objects under their stream-time names. Turning these into final
/// changed/deleted path lists is the resolver's job.
#[derive(Clone, Debug)]
pub struct Op {
    pub seq: u64,
    pub kind: OpKind,
    pub path: Vec<u8>,
    /// rename: the destination path; link: the existing path the new
    /// name is linked from.
    pub aux: Option<Vec<u8>>,
}

pub struct StreamReader<R: Read> {
    inner: R,
    pub version: u32,
    seq: u64,
    payload: Vec<u8>,
    ended: bool,
    pub saw_end: bool,
    pub commands: u64,
}

/// Read until `buf` is full or EOF; returns the number of bytes read.
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(m) => n += m,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

impl<R: Read> StreamReader<R> {
    pub fn new(mut inner: R) -> Result<Self, StreamError> {
        let mut magic = [0u8; 13];
        let n = read_up_to(&mut inner, &mut magic)?;
        if n < magic.len() {
            return Err(StreamError::Truncated("stream header"));
        }
        if &magic != STREAM_MAGIC {
            return Err(StreamError::BadMagic);
        }
        let mut vbuf = [0u8; 4];
        let n = read_up_to(&mut inner, &mut vbuf)?;
        if n < vbuf.len() {
            return Err(StreamError::Truncated("stream version"));
        }
        let version = u32::from_le_bytes(vbuf);
        if version != 1 && version != 2 {
            return Err(StreamError::UnsupportedVersion(version));
        }
        Ok(StreamReader {
            inner,
            version,
            seq: 0,
            payload: Vec::new(),
            ended: false,
            saw_end: false,
            commands: 0,
        })
    }

    /// Read and verify one raw command. Returns None at a clean end
    /// of stream (the sentinel command, or EOF exactly on a command
    /// boundary after the sentinel). `payload` borrows the reader's
    /// reusable buffer.
    fn next_command(&mut self) -> Result<Option<(u16, &[u8])>, StreamError> {
        if self.ended {
            // After the sentinel, only EOF is acceptable.
            let mut b = [0u8; 1];
            let n = read_up_to(&mut self.inner, &mut b)?;
            if n != 0 {
                // Count the rest without buffering it.
                let mut extra = n;
                let mut buf = [0u8; 8192];
                loop {
                    let m = read_up_to(&mut self.inner, &mut buf)?;
                    if m == 0 {
                        break;
                    }
                    extra += m;
                }
                return Err(StreamError::TrailingBytesAfterEnd(extra));
            }
            return Ok(None);
        }
        let mut hdr = [0u8; 10];
        let n = read_up_to(&mut self.inner, &mut hdr)?;
        if n == 0 {
            // EOF on a command boundary. A stream that stops here
            // without the sentinel was truncated (or its producer
            // died mid-send); treat it as corrupt, not as a short
            // but valid diff.
            return Err(StreamError::Truncated(
                "end of stream without the end-of-stream command",
            ));
        }
        if n < hdr.len() {
            return Err(StreamError::Truncated("command header"));
        }
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let cmd = u16::from_le_bytes(hdr[4..6].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(hdr[6..10].try_into().unwrap());
        if len > MAX_CMD_LEN {
            return Err(StreamError::CommandTooLarge(len));
        }
        self.payload.clear();
        self.payload.resize(len as usize, 0);
        let n = read_up_to(&mut self.inner, &mut self.payload)?;
        if n < len as usize {
            return Err(StreamError::Truncated("command payload"));
        }
        let mut crc = Crc32c::new();
        let mut zeroed = hdr;
        zeroed[6..10].fill(0);
        crc.update(&zeroed);
        crc.update(&self.payload);
        let computed = crc.finish();
        if computed != stored_crc {
            return Err(StreamError::CrcMismatch {
                cmd,
                stored: stored_crc,
                computed,
            });
        }
        self.commands += 1;
        if cmd == CMD_END_OR_FALLOCATE && len == 0 {
            self.ended = true;
            self.saw_end = true;
            return self.next_command();
        }
        Ok(Some((cmd, &self.payload)))
    }

    /// Iterate the attribute block of one command payload.
    fn attrs<'a>(
        cmd: u16,
        payload: &'a [u8],
    ) -> Result<impl Iterator<Item = (u16, &'a [u8])>, StreamError> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < payload.len() {
            if payload.len() - pos < 4 {
                return Err(StreamError::MalformedAttrs(cmd));
            }
            let atype = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap());
            let alen = u16::from_le_bytes(payload[pos + 2..pos + 4].try_into().unwrap())
                as usize;
            pos += 4;
            if payload.len() - pos < alen {
                return Err(StreamError::MalformedAttrs(cmd));
            }
            out.push((atype, &payload[pos..pos + alen]));
            pos += alen;
        }
        Ok(out.into_iter())
    }

    fn get_attr(cmd: u16, payload: &[u8], attr: u16) -> Result<Vec<u8>, StreamError> {
        for (t, v) in Self::attrs(cmd, payload)? {
            if t == attr {
                return Ok(v.to_vec());
            }
        }
        Err(StreamError::MissingAttr { cmd, attr })
    }

    fn opt_attr(cmd: u16, payload: &[u8], attr: u16) -> Result<Option<Vec<u8>>, StreamError> {
        for (t, v) in Self::attrs(cmd, payload)? {
            if t == attr {
                return Ok(Some(v.to_vec()));
            }
        }
        Ok(None)
    }

    /// Read the next classified operation, skipping the snapshot /
    /// subvol framing commands. Returns None at the end of stream.
    pub fn next_op(&mut self) -> Result<Option<Op>, StreamError> {
        loop {
            let Some((cmd, payload)) = self.next_command()? else {
                return Ok(None);
            };
            // Copy what we need out of the borrowed payload before
            // the next read reuses the buffer.
            let classified = match cmd {
                CMD_SUBVOL | CMD_SNAPSHOT => None,
                CMD_MKFILE | CMD_MKDIR | CMD_MKNOD | CMD_MKFIFO | CMD_MKSOCK
                | CMD_SYMLINK => Some((
                    OpKind::Create,
                    Self::get_attr(cmd, payload, ATTR_PATH)?,
                    None,
                )),
                CMD_RENAME => Some((
                    OpKind::Rename,
                    Self::get_attr(cmd, payload, ATTR_PATH)?,
                    Some(Self::get_attr(cmd, payload, ATTR_PATH_TO)?),
                )),
                CMD_LINK => Some((
                    OpKind::Link,
                    Self::get_attr(cmd, payload, ATTR_PATH)?,
                    Some(Self::get_attr(cmd, payload, ATTR_PATH_LINK)?),
                )),
                CMD_UNLINK | CMD_RMDIR => Some((
                    OpKind::Delete,
                    Self::get_attr(cmd, payload, ATTR_PATH)?,
                    None,
                )),
                CMD_SET_XATTR | CMD_REMOVE_XATTR | CMD_WRITE | CMD_CLONE
                | CMD_TRUNCATE | CMD_SET_MODE | CMD_SET_OWNER | CMD_SET_TIMES
                | CMD_UPDATE_EXTENT => Some((
                    OpKind::Modify,
                    Self::get_attr(cmd, payload, ATTR_PATH)?,
                    None,
                )),
                CMD_END_OR_FALLOCATE => {
                    // A payload-carrying cmd 21 is a v2 fallocate:
                    // a content change on its path.
                    match Self::opt_attr(cmd, payload, ATTR_PATH)? {
                        Some(p) => Some((OpKind::Modify, p, None)),
                        None => None,
                    }
                }
                other => return Err(StreamError::UnknownCommand(other)),
            };
            if let Some((kind, path, aux)) = classified {
                self.seq += 1;
                return Ok(Some(Op {
                    seq: self.seq,
                    kind,
                    path,
                    aux,
                }));
            }
        }
    }

    /// Drain the stream, collecting every operation.
    pub fn read_all(&mut self) -> Result<Vec<Op>, StreamError> {
        let mut ops = Vec::new();
        while let Some(op) = self.next_op()? {
            ops.push(op);
        }
        Ok(ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc32c::Crc32c;
    use std::io::Cursor;

    fn attr(t: u16, v: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&t.to_le_bytes());
        out.extend_from_slice(&(v.len() as u16).to_le_bytes());
        out.extend_from_slice(v);
        out
    }

    fn command(cmd: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&cmd.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut crc = Crc32c::new();
        crc.update(&out);
        crc.update(payload);
        let c = crc.finish();
        out[6..10].copy_from_slice(&c.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn stream(cmds: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(STREAM_MAGIC);
        out.extend_from_slice(&1u32.to_le_bytes());
        for (cmd, payload) in cmds {
            out.extend_from_slice(&command(*cmd, payload));
        }
        out.extend_from_slice(&command(CMD_END_OR_FALLOCATE, &[]));
        out
    }

    #[test]
    fn parses_a_small_stream() {
        let snap = {
            let mut p = Vec::new();
            p.extend(attr(ATTR_PATH, b"fresh"));
            p.extend(attr(1, &[0u8; 16])); // uuid
            p
        };
        let mut ren = Vec::new();
        ren.extend(attr(ATTR_PATH, b"dirB"));
        ren.extend(attr(ATTR_PATH_TO, b"dirB-moved"));
        let mut un = Vec::new();
        un.extend(attr(ATTR_PATH, b"o260-1785-0/a.txt"));
        let data = stream(&[
            (CMD_SNAPSHOT, snap),
            (CMD_RENAME, ren),
            (CMD_UNLINK, un),
        ]);
        let mut r = StreamReader::new(Cursor::new(data)).unwrap();
        let ops = r.read_all().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].kind, OpKind::Rename);
        assert_eq!(ops[0].path, b"dirB");
        assert_eq!(ops[0].aux.as_deref(), Some(b"dirB-moved".as_slice()));
        assert_eq!(ops[1].kind, OpKind::Delete);
        assert!(r.saw_end);
    }

    #[test]
    fn crc_mismatch_is_an_error() {
        let mut data = stream(&[(CMD_UNLINK, attr(ATTR_PATH, b"x"))]);
        // Flip a payload byte (inside the path value).
        let i = data.len() - 12;
        data[i] ^= 0xFF;
        let mut r = StreamReader::new(Cursor::new(data)).unwrap();
        match r.read_all() {
            Err(StreamError::CrcMismatch { .. }) => {}
            other => panic!("expected crc mismatch, got {other:?}"),
        }
    }

    #[test]
    fn missing_end_is_truncation() {
        let mut data = stream(&[(CMD_UNLINK, attr(ATTR_PATH, b"x"))]);
        data.truncate(data.len() - 10); // drop the END command
        let mut r = StreamReader::new(Cursor::new(data)).unwrap();
        match r.read_all() {
            Err(StreamError::Truncated(_)) => {}
            other => panic!("expected truncation, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command_is_an_error() {
        let data = stream(&[(99, attr(ATTR_PATH, b"x"))]);
        let mut r = StreamReader::new(Cursor::new(data)).unwrap();
        match r.read_all() {
            Err(StreamError::UnknownCommand(99)) => {}
            other => panic!("expected unknown command, got {other:?}"),
        }
    }
}
