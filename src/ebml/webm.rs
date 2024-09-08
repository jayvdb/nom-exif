use std::{
    collections::HashMap,
    fmt::Debug,
    io::{BufRead, Cursor},
};

use bytes::Buf;
use chrono::{DateTime, NaiveDate, Utc};
use thiserror::Error;

use crate::{
    ebml::element::{
        find_element_by_id, get_as_f64, get_as_u64, next_element_header, parse_ebml_doc_type,
        EBMLGlobalId, TopElementId,
    },
    error::{ParsedError, ParsingError},
    loader::Load,
};

use super::{
    element::{
        travel_while, ElementHeader, ParseEBMLFailed, UnknowEbmlIDError, INVALID_ELEMENT_ID,
    },
    vint::{ParseVIntFailed, VInt},
};

#[derive(Debug, Clone, Default)]
pub struct EbmlFileInfo {
    doc_type: String,
    segment_info: SegmentInfo,
    tracks_info: TracksInfo,
}

#[derive(Debug, Error)]
pub enum ParseWebmFailed {
    #[error("need more bytes: {0}")]
    Need(usize),

    #[error("not an WEBM file")]
    NotWebmFile,

    #[error("invalid WEBM file: {0}")]
    InvalidWebmFile(Box<dyn std::error::Error>),

    #[error("invalid seek entry")]
    InvalidSeekEntry,
}

/// Parse EBML based files, e.g.: `.webm`, `.mkv`, etc.
///
/// Refer to:
/// - [Matroska Elements](https://www.matroska.org/technical/elements.html)
/// - [EBML Specification](https://github.com/ietf-wg-cellar/ebml-specification/blob/master/specification.markdown)
#[tracing::instrument(skip_all)]
pub(crate) fn parse_webm<T: Load>(mut loader: T) -> Result<EbmlFileInfo, ParsedError> {
    let mut pos: usize = 0;
    let doc_type = loader.load_and_parse(|input| {
        tracing::debug!(len = input.len(), "buf size");
        let mut cursor = Cursor::new(input);
        let doc_type = parse_ebml_doc_type(&mut cursor)?;
        pos = cursor.position() as usize;
        Ok(doc_type)
    })?;

    tracing::debug!(doc_type, pos);

    let at = pos;
    let pos = loader.load_and_parse_at(
        |input, at| {
            let mut cursor = Cursor::new(&input[at..]);
            let header = next_element_header(&mut cursor)?;
            tracing::debug!(segment_header = ?header);
            if header.id != TopElementId::Segment as u64 {
                return Err(ParseWebmFailed::NotWebmFile.into());
            }
            pos = at + cursor.position() as usize;
            Ok(pos)
        },
        at,
    )?;

    let mut file_info = EbmlFileInfo {
        doc_type,
        ..Default::default()
    };

    if let Ok(seeks) = loader.load_and_parse_at(parse_seeks, pos) {
        let info_seek = seeks.get(&(SegmentId::Info as u32)).cloned();
        let tracks_seek = seeks.get(&(SegmentId::Tracks as u32)).cloned();
        if let Some(pos) = info_seek {
            let info = loader.load_and_parse_at(parse_segment_info, pos as usize)?;
            tracing::debug!(?info);
            if let Some(info) = info {
                file_info.segment_info = info;
            }
        }
        if let Some(pos) = tracks_seek {
            let tracks =
                loader.load_and_parse_at(|x, at| Ok(parse_tracks_info(x, at)?), pos as usize)?;
            tracing::debug!(?tracks);
            if let Some(info) = tracks {
                file_info.tracks_info = info;
            }
        }
    } else {
        // According to the specification, The first Info Element SHOULD occur
        // before the first Tracks Element
        let info: Option<SegmentInfo> = loader.load_and_parse_at(
            |x, at| {
                let mut cursor = Cursor::new(&x[at..]);
                let header = travel_while(&mut cursor, |h| h.id != SegmentId::Info as u64)?;
                parse_segment_info(
                    &x[at + cursor.position() as usize - header.header_size..],
                    0,
                )
            },
            pos,
        )?;
        tracing::debug!(?info);
        if let Some(info) = info {
            file_info.segment_info = info;
        }

        let track = loader.load_and_parse_at(
            |x, at| {
                let mut cursor = Cursor::new(&x[at..]);
                let header = travel_while(&mut cursor, |h| h.id != SegmentId::Tracks as u64)?;
                Ok(parse_tracks_info(
                    &x[at + cursor.position() as usize - header.header_size..],
                    0,
                )?)
            },
            pos,
        )?;
        tracing::debug!(?track);
        if let Some(info) = track {
            file_info.tracks_info = info;
        }
    }

    Ok(file_info)
}

#[derive(Debug, Clone, Default)]
struct TracksInfo {
    width: usize,
    height: usize,
}

#[tracing::instrument(skip(input))]
fn parse_tracks_info(input: &[u8], pos: usize) -> Result<Option<TracksInfo>, ParseWebmFailed> {
    if pos >= input.len() {
        return Err(ParseWebmFailed::Need(pos - input.len() + 1));
    }
    let mut cursor = Cursor::new(&input[pos..]);
    let header = next_element_header(&mut cursor)?;
    tracing::debug!(tracks_info_header = ?header);

    if cursor.remaining() < header.data_size {
        return Err(ParseWebmFailed::Need(header.data_size - cursor.remaining()));
    }

    let mut cursor = Cursor::new(&cursor.chunk()[..header.data_size]);
    let header = travel_while(&mut cursor, |h| h.id == TracksId::VideoTrack as u64)?;
    tracing::debug!(?header, "video track");

    if cursor.remaining() < header.data_size {
        return Err(ParseWebmFailed::Need(header.data_size - cursor.remaining()));
    }

    match parse_track(&cursor.chunk()[..header.data_size]).map(|x| {
        x.map(|x| TracksInfo {
            width: x.width,
            height: x.height,
        })
    }) {
        Ok(x) => Ok(x),
        // Don't bubble Need error to caller here
        Err(ParseWebmFailed::Need(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn parse_track(input: &[u8]) -> Result<Option<VideoTrackInfo>, ParseWebmFailed> {
    let mut cursor = Cursor::new(input);

    while cursor.has_remaining() {
        let header = next_element_header(&mut cursor)?;
        tracing::debug!(?header, "track sub-element");

        let id = TryInto::<TracksId>::try_into(header.id);
        let pos = cursor.position() as usize;
        cursor.consume(header.data_size);

        let Ok(id) = id else {
            continue;
        };

        if id == TracksId::VideoTrack {
            return parse_video_track(&input[pos..pos + header.data_size]).map(Some);
        }
    }
    Ok(None)
}

fn parse_video_track(input: &[u8]) -> Result<VideoTrackInfo, ParseWebmFailed> {
    let mut cursor = Cursor::new(input);
    let mut info = VideoTrackInfo::default();

    let header = travel_while(&mut cursor, |h| h.id != TracksId::PixelWidth as u64)?;
    tracing::debug!(?header, "video track width element");
    if let Some(v) = get_as_u64(&mut cursor, header.data_size) {
        info.width = v as usize;
    }

    // search from beginning
    cursor.set_position(0);
    let header = travel_while(&mut cursor, |h| h.id != TracksId::PixelHeight as u64)?;
    tracing::debug!(?header, "video track height element");
    if let Some(v) = get_as_u64(&mut cursor, header.data_size) {
        info.height = v as usize;
    }

    Ok(info)
}

#[derive(Debug, Clone, Default)]
struct VideoTrackInfo {
    width: usize,
    height: usize,
}

#[derive(Debug, Clone, Default)]
struct SegmentInfo {
    // in nano seconds
    duration: f64,
    date: DateTime<Utc>,
}

#[tracing::instrument(skip(input))]
fn parse_segment_info(input: &[u8], pos: usize) -> Result<Option<SegmentInfo>, ParsingError> {
    if pos >= input.len() {
        return Err(ParsingError::Need(pos - input.len() + 1));
    }
    let mut cursor = Cursor::new(&input[pos..]);
    let header = next_element_header(&mut cursor)?;
    tracing::debug!(segment_info_header = ?header);

    if cursor.remaining() < header.data_size {
        return Err(ParsingError::Need(header.data_size - cursor.remaining()));
    }

    let mut cursor = Cursor::new(&cursor.chunk()[..header.data_size]);
    match parse_segment_info_body(&mut cursor) {
        Ok(x) => Ok(Some(x)),
        // Don't bubble Need error to caller here
        Err(ParsingError::Need(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn parse_segment_info_body(cursor: &mut Cursor<&[u8]>) -> Result<SegmentInfo, ParsingError> {
    // timestamp in nanosecond = element value * TimestampScale
    // By default, one segment tick represents one millisecond
    let mut time_scale = 1_000_000;
    let mut info = SegmentInfo::default();

    while cursor.has_remaining() {
        let header = next_element_header(cursor)?;
        let id = TryInto::<InfoId>::try_into(header.id);
        tracing::debug!(?header, "segment info sub-element");

        if let Ok(id) = id {
            match id {
                InfoId::TimestampScale => {
                    if let Some(v) = get_as_u64(cursor, header.data_size) {
                        time_scale = v;
                    }
                }
                InfoId::Duration => {
                    if let Some(v) = get_as_f64(cursor, header.data_size) {
                        info.duration = v * time_scale as f64;
                    }
                }
                InfoId::Date => {
                    if let Some(v) = get_as_u64(cursor, header.data_size) {
                        // webm date is a 2001 based timestamp
                        let dt = NaiveDate::from_ymd_opt(2001, 1, 1)
                            .unwrap()
                            .and_hms_opt(0, 0, 0)
                            .unwrap()
                            .and_utc();
                        let diff = dt - DateTime::from_timestamp_nanos(0);
                        info.date = DateTime::from_timestamp_nanos(v as i64) + diff;
                    }
                }
            }
        } else {
            cursor.consume(header.data_size);
        }
    }

    Ok(info)
}

fn parse_seeks(input: &[u8], pos: usize) -> Result<HashMap<u32, u64>, ParsingError> {
    let mut cursor = Cursor::new(&input[pos..]);
    // find SeekHead element
    let header = find_element_by_id(&mut cursor, SegmentId::SeekHead as u64)?;
    tracing::debug!(segment_header = ?header);
    if cursor.remaining() < header.data_size {
        return Err(ParsingError::Need(header.data_size - cursor.remaining()));
    }

    let header_pos = pos + cursor.position() as usize - header.header_size;
    let mut cur = Cursor::new(&cursor.chunk()[..header.data_size]);
    let mut seeks = parse_seek_head(&mut cur)?;
    for (_, pos) in seeks.iter_mut() {
        *pos += header_pos as u64;
    }
    Ok(seeks)
}

#[derive(Clone)]
struct SeekEntry {
    seek_id: u32,
    seek_pos: u64,
}

impl Debug for SeekEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = self.seek_id as u64;
        let s = TryInto::<TopElementId>::try_into(id)
            .map(|x| format!("{x:?}"))
            .or_else(|_| TryInto::<SegmentId>::try_into(id).map(|x| format!("{x:?}")))
            .unwrap_or_else(|_| format!("0x{:04x}", id));
        f.debug_struct("SeekEntry")
            .field("seekId", &s)
            .field("seekPosition", &self.seek_pos.to_string())
            .finish()
    }
}

#[tracing::instrument(skip_all)]
fn parse_seek_head(input: &mut Cursor<&[u8]>) -> Result<HashMap<u32, u64>, ParseWebmFailed> {
    let mut entries = HashMap::new();
    while input.has_remaining() {
        match parse_seek_entry(input) {
            Ok(Some(entry)) => {
                tracing::debug!(seek_entry=?entry);
                entries.insert(entry.seek_id, entry.seek_pos);
            }
            Ok(None) => {
                // tracing::debug!("Void or Crc32 Element");
            }
            Err(ParseWebmFailed::InvalidSeekEntry) => {
                tracing::debug!("ignore invalid seek entry");
            }
            Err(e) => return Err(e),
        };
    }
    Ok(entries)
}

fn parse_seek_entry(input: &mut Cursor<&[u8]>) -> Result<Option<SeekEntry>, ParseWebmFailed> {
    // 0xFF is an invalid ID
    let mut seek_id = INVALID_ELEMENT_ID as u32;
    let mut seek_pos = 0u64;

    let id = VInt::as_u64_with_marker(input)?;
    let data_size = VInt::as_usize(input)?;
    if input.remaining() < data_size {
        return Err(ParseWebmFailed::Need(data_size - input.remaining()));
    }

    if id != SeekHeadId::Seek as u64 {
        input.consume(data_size);
        if id == EBMLGlobalId::Crc32 as u64 || id == EBMLGlobalId::Void as u64 {
            return Ok(None);
        }
        tracing::debug!(
            id = format!("0x{id:x}"),
            "{}",
            ParseWebmFailed::InvalidSeekEntry
        );
        return Err(ParseWebmFailed::InvalidSeekEntry);
    }

    let pos = input.position() as usize;
    input.consume(data_size);
    let mut buf = Cursor::new(&input.get_ref()[pos..pos + data_size]);

    while buf.has_remaining() {
        let id = VInt::as_u64_with_marker(&mut buf)?;
        let size = VInt::as_usize(&mut buf)?;

        match id {
            x if x == SeekHeadId::SeekId as u64 => {
                seek_id = VInt::as_u64_with_marker(&mut buf)? as u32;
            }
            x if x == SeekHeadId::SeekPosition as u64 => {
                if size == 8 {
                    seek_pos = buf.get_u64();
                } else if size == 4 {
                    seek_pos = buf.get_u32() as u64;
                } else {
                    return Err(ParseWebmFailed::InvalidSeekEntry);
                }
            }
            _ => {
                return Err(ParseWebmFailed::InvalidSeekEntry);
            }
        }

        if seek_id != INVALID_ELEMENT_ID as u32 && seek_pos != 0 {
            break;
        }
    }

    if seek_id == INVALID_ELEMENT_ID as u32 || seek_pos == 0 {
        return Err(ParseWebmFailed::InvalidSeekEntry);
    }

    Ok(Some(SeekEntry { seek_id, seek_pos }))
}

#[derive(Debug, Clone, Copy)]
enum SegmentId {
    SeekHead = 0x114D9B74,
    Info = 0x1549A966,
    Tracks = 0x1654AE6B,
    Cluster = 0x1F43B675,
    Cues = 0x1C53BB6B,
}

#[derive(Debug, Clone, Copy)]
enum InfoId {
    TimestampScale = 0x2AD7B1,
    Duration = 0x4489,
    Date = 0x4461,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TracksId {
    TrackEntry = 0xAE,
    TrackType = 0x83,
    VideoTrack = 0xE0,
    PixelWidth = 0xB0,
    PixelHeight = 0xBA,
}

impl TryFrom<u64> for TracksId {
    type Error = UnknowEbmlIDError;
    fn try_from(v: u64) -> Result<Self, Self::Error> {
        let id = match v {
            x if x == Self::TrackEntry as u64 => Self::TrackEntry,
            x if x == Self::TrackType as u64 => Self::TrackType,
            x if x == Self::VideoTrack as u64 => Self::VideoTrack,
            x if x == Self::PixelWidth as u64 => Self::PixelWidth,
            x if x == Self::PixelHeight as u64 => Self::PixelHeight,
            o => return Err(UnknowEbmlIDError(o)),
        };
        Ok(id)
    }
}

impl TryFrom<u64> for InfoId {
    type Error = UnknowEbmlIDError;
    fn try_from(v: u64) -> Result<Self, Self::Error> {
        let id = match v {
            x if x == Self::TimestampScale as u64 => Self::TimestampScale,
            x if x == Self::Duration as u64 => Self::Duration,
            x if x == Self::Date as u64 => Self::Date,
            o => return Err(UnknowEbmlIDError(o)),
        };
        Ok(id)
    }
}

#[derive(Debug, Clone, Copy)]
enum SeekHeadId {
    Seek = 0x4DBB,
    SeekId = 0x53AB,
    SeekPosition = 0x53AC,
}

impl SegmentId {
    fn code(self) -> u32 {
        self as u32
    }
}

impl TryFrom<u64> for SegmentId {
    type Error = UnknowEbmlIDError;
    fn try_from(v: u64) -> Result<Self, Self::Error> {
        let id = match v {
            x if x == Self::SeekHead as u64 => Self::SeekHead,
            x if x == Self::Info as u64 => Self::Info,
            x if x == Self::Tracks as u64 => Self::Tracks,
            x if x == Self::Cluster as u64 => Self::Cluster,
            x if x == Self::Cues as u64 => Self::Cues,
            o => return Err(UnknowEbmlIDError(o)),
        };
        Ok(id)
    }
}

impl Debug for ElementHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = TryInto::<TopElementId>::try_into(self.id)
            .map(|x| format!("{x:?}"))
            .or_else(|_| TryInto::<SegmentId>::try_into(self.id).map(|x| format!("{x:?}")))
            .or_else(|_| TryInto::<InfoId>::try_into(self.id).map(|x| format!("{x:?}")))
            .or_else(|_| TryInto::<TracksId>::try_into(self.id).map(|x| format!("{x:?}")))
            .unwrap_or_else(|_| format!("0x{:04x}", self.id));
        f.debug_struct("ElementHeader")
            .field("id", &s)
            .field("data_size", &self.data_size.to_string())
            .finish()
    }
}

impl From<ParseEBMLFailed> for ParseWebmFailed {
    fn from(value: ParseEBMLFailed) -> Self {
        match value {
            ParseEBMLFailed::Need(i) => Self::Need(i),
            ParseEBMLFailed::NotEBMLFile => Self::NotWebmFile,
            ParseEBMLFailed::InvalidEBMLFile(e) => Self::InvalidWebmFile(e),
        }
    }
}

impl From<ParseEBMLFailed> for ParsingError {
    fn from(value: ParseEBMLFailed) -> Self {
        match value {
            ParseEBMLFailed::Need(i) => ParsingError::Need(i),
            ParseEBMLFailed::NotEBMLFile | ParseEBMLFailed::InvalidEBMLFile(_) => {
                ParsingError::Failed(value.to_string())
            }
        }
    }
}

impl From<ParseVIntFailed> for ParseWebmFailed {
    fn from(value: ParseVIntFailed) -> Self {
        match value {
            ParseVIntFailed::InvalidVInt(e) => Self::InvalidWebmFile(e.into()),
            ParseVIntFailed::Need(i) => Self::Need(i),
        }
    }
}

impl From<ParseVIntFailed> for ParsingError {
    fn from(value: ParseVIntFailed) -> Self {
        match value {
            ParseVIntFailed::InvalidVInt(_) => Self::Failed(value.to_string()),
            ParseVIntFailed::Need(i) => Self::Need(i),
        }
    }
}

impl From<ParseWebmFailed> for ParsingError {
    fn from(value: ParseWebmFailed) -> Self {
        match value {
            ParseWebmFailed::NotWebmFile
            | ParseWebmFailed::InvalidWebmFile(_)
            | ParseWebmFailed::InvalidSeekEntry => Self::Failed(value.to_string()),
            ParseWebmFailed::Need(n) => Self::Need(n),
        }
    }
}
