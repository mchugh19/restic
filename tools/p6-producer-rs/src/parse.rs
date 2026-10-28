//! Stream -> lists: read a send stream incrementally, collect the
//! classified operations, and resolve them against the two
//! snapshots into the final changed/deleted relative path sets.

use std::collections::HashSet;
use std::io::{self, Read};
use std::path::Path;

use crate::resolve::{Event, Resolver};
use crate::stream::{StreamError, StreamReader};

#[derive(Debug)]
pub enum ParseError {
    Stream(StreamError),
    Io(io::Error),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Stream(e) => write!(f, "{e}"),
            ParseError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<StreamError> for ParseError {
    fn from(e: StreamError) -> Self {
        ParseError::Stream(e)
    }
}

impl From<io::Error> for ParseError {
    fn from(e: io::Error) -> Self {
        ParseError::Io(e)
    }
}

pub struct ParseOutcome {
    pub changed: HashSet<Vec<u8>>,
    pub deleted: HashSet<Vec<u8>>,
    /// Changed candidates the resolver proved atime-only and
    /// dropped (see resolve.rs); not part of either list.
    pub atime_only: u64,
    pub events: Vec<Event>,
    pub version: u32,
    pub commands: u64,
}

pub fn parse_stream<R: Read>(
    src: R,
    retained: &Path,
    fresh: &Path,
) -> Result<ParseOutcome, ParseError> {
    let mut reader = StreamReader::new(src)?;
    let ops = reader.read_all()?;
    let version = reader.version;
    let commands = reader.commands;
    let mut resolver = Resolver::new(retained.to_path_buf(), fresh.to_path_buf())?;
    let (changed, deleted, atime_only) = resolver.resolve(&ops)?;
    Ok(ParseOutcome {
        changed,
        deleted,
        atime_only,
        events: resolver.events,
        version,
        commands,
    })
}
