//! Pipe protocol between the app and the indexer: versioned
//! JSON messages, each framed by a little-endian `u32` byte length.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Bumped on any incompatible message change; exchanged in `Hello`.
pub const PROTOCOL_VERSION: u32 = 2;
/// The service's pipe.
pub const PIPE_NAME: &str = r"\\.\pipe\flint-index";
/// Largest frame either side accepts; a bigger one closes the connection.
pub const MAX_FRAME: usize = 1 << 20;
/// Largest result count a query may ask for.
pub const MAX_LIMIT: u32 = 200;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum ClientMsg {
    Hello {
        version: u32,
    },
    /// A new query cancels the connection's previous one.
    Query {
        seq: u64,
        text: String,
        limit: u32,
        /// Frecency weights in [0, 1] by full path.
        #[serde(default)]
        boosts: Vec<(String, f32)>,
    },
    Status,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WireHit {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub typo: bool,
    /// Half-open UTF-16 ranges on `name`.
    pub highlights: Vec<(u32, u32)>,
    /// Unix seconds; 0 when unknown.
    pub last_change: u32,
    /// The ranking score (higher is better), comparable between indexers
    /// given the same boosts: the app merges two sources by it (v2).
    pub score: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VolumeStatus {
    pub drive: String,
    /// "scanning", "live" or "basic" (folder walk, no change journal).
    pub state: String,
    pub entries: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum ServerMsg {
    Welcome {
        version: u32,
        /// "service" or "user".
        mode: String,
    },
    Results {
        seq: u64,
        total: u32,
        elapsed_us: u32,
        hits: Vec<WireHit>,
    },
    Cancelled {
        seq: u64,
    },
    StatusReply {
        entries: u64,
        volumes: Vec<VolumeStatus>,
    },
    Error {
        code: String,
        message: String,
    },
}

/// Writes one frame.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Reads one frame; a length above `MAX_FRAME` is an error before any
/// allocation.
pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {n} bytes exceeds the limit"),
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn send<M: Serialize>(w: &mut impl Write, m: &M) -> io::Result<()> {
    write_frame(w, &serde_json::to_vec(m).map_err(io::Error::other)?)
}

pub fn recv<M: for<'de> Deserialize<'de>>(r: &mut impl Read) -> io::Result<M> {
    let f = read_frame(r)?;
    serde_json::from_slice(&f).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_roundtrip_through_frames() {
        let msgs = vec![
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
            },
            ClientMsg::Query {
                seq: 7,
                text: "bütçe".into(),
                limit: 10,
                boosts: vec![(r"C:\a\b.txt".into(), 0.5)],
            },
            ClientMsg::Status,
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            send(&mut buf, m).unwrap();
        }
        let mut r = &buf[..];
        for m in &msgs {
            assert_eq!(&recv::<ClientMsg>(&mut r).unwrap(), m);
        }
    }

    #[test]
    fn wire_format_is_tagged_json() {
        let j = serde_json::to_string(&ServerMsg::Cancelled { seq: 3 }).unwrap();
        assert_eq!(j, r#"{"type":"Cancelled","seq":3}"#);
        // boosts may be omitted by a minimal client
        let q: ClientMsg =
            serde_json::from_str(r#"{"type":"Query","seq":1,"text":"a","limit":5}"#).unwrap();
        assert!(matches!(q, ClientMsg::Query { boosts, .. } if boosts.is_empty()));
    }

    #[test]
    fn oversized_and_truncated_frames_fail() {
        let mut big = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        big.extend([0u8; 16]);
        assert_eq!(
            read_frame(&mut &big[..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let short = [5u8, 0, 0, 0, b'{'];
        assert_eq!(
            read_frame(&mut &short[..]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(write_frame(&mut Vec::new(), &vec![0u8; MAX_FRAME + 1]).is_err());
    }
}
