use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    ptr::NonNull,
};

use rusqlite::{Connection, MAIN_DB, ffi, serialize::OwnedData};

use crate::StorageError;

const DATABASE_HEADER_BYTES: usize = 100;
const WAL_HEADER_BYTES: u64 = 32;
const WAL_FRAME_HEADER_BYTES: u64 = 24;
const WAL_FORMAT_VERSION: u32 = 3_007_000;
const WAL_MAGIC: u32 = 0x377f_0682;
const MAX_DATABASE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_WAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChecksumOrder {
    LittleEndian,
    BigEndian,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalHeader {
    page_size: u32,
    salt: [u8; 8],
    checksum_order: ChecksumOrder,
    checksum: [u32; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalPlan {
    header: WalHeader,
    last_committed_frame: u64,
    final_pages: u32,
}

#[derive(Debug)]
enum WalError {
    Io(io::Error),
    Corrupt(&'static str),
    Limit(&'static str),
}

impl From<io::Error> for WalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(super) fn open_replayed(path: &Path, wal_path: &Path) -> Result<Connection, StorageError> {
    let mut database = File::open(path).map_err(io_error)?;
    let database_len = database.metadata().map_err(io_error)?.len();
    let main_page_size =
        inspect_main_database(&mut database, database_len).map_err(storage_error)?;
    let main_pages = u32::try_from(database_len / u64::from(main_page_size))
        .map_err(|_| corrupt("state database page count exceeds the inspection limit"))?;

    let mut wal = File::open(wal_path).map_err(io_error)?;
    let wal_len = wal.metadata().map_err(io_error)?.len();
    let plan = analyze_wal(&mut wal, wal_len, main_page_size, main_pages).map_err(storage_error)?;
    let (wal_pages, main_page_limit) =
        index_wal(&mut wal, wal_len, plan, main_pages).map_err(storage_error)?;
    let final_len = u64::from(plan.final_pages)
        .checked_mul(u64::from(main_page_size))
        .ok_or_else(|| resource_limit("replayed state database size overflowed"))?;
    if final_len == 0 || final_len > MAX_DATABASE_BYTES {
        return Err(resource_limit(
            "replayed state database exceeds the inspection limit",
        ));
    }

    database.seek(SeekFrom::Start(0)).map_err(io_error)?;
    wal.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let reader = ReplayedDatabase {
        database,
        wal,
        wal_pages,
        main_page_limit,
        main_len: database_len,
        page_size: u64::from(main_page_size),
        position: 0,
    };
    let final_len = usize::try_from(final_len)
        .map_err(|_| resource_limit("replayed state database size overflowed"))?;
    let raw = unsafe { ffi::sqlite3_malloc64(final_len as u64) }.cast::<u8>();
    let pointer = NonNull::new(raw)
        .ok_or_else(|| resource_limit("unable to allocate the replayed state database"))?;
    let data = unsafe { OwnedData::from_raw_nonnull(pointer, final_len) };
    let buffer = unsafe { std::slice::from_raw_parts_mut(pointer.as_ptr(), final_len) };
    let mut reader = reader;
    reader.read_exact(buffer).map_err(io_error)?;
    let mut connection = Connection::open_in_memory().map_err(database_error)?;
    connection
        .deserialize(MAIN_DB, data, true)
        .map_err(database_error)?;
    Ok(connection)
}

fn inspect_main_database(database: &mut File, database_len: u64) -> Result<u32, WalError> {
    if !(DATABASE_HEADER_BYTES as u64..=MAX_DATABASE_BYTES).contains(&database_len) {
        return Err(WalError::Limit(
            "state database exceeds the inspection limit",
        ));
    }
    let mut header = [0_u8; DATABASE_HEADER_BYTES];
    database.read_exact(&mut header)?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(WalError::Corrupt("state database header is invalid"));
    }
    let encoded_page_size = u16::from_be_bytes([header[16], header[17]]);
    let page_size = if encoded_page_size == 1 {
        65_536
    } else {
        u32::from(encoded_page_size)
    };
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(WalError::Corrupt("state database page size is invalid"));
    }
    if !database_len.is_multiple_of(u64::from(page_size)) {
        return Err(WalError::Corrupt(
            "state database length is not page aligned",
        ));
    }
    Ok(page_size)
}

fn analyze_wal<R: Read + Seek>(
    wal: &mut R,
    wal_len: u64,
    main_page_size: u32,
    main_pages: u32,
) -> Result<WalPlan, WalError> {
    if !(WAL_HEADER_BYTES..=MAX_WAL_BYTES).contains(&wal_len) {
        return Err(WalError::Limit(
            "state database WAL exceeds the inspection limit",
        ));
    }
    wal.seek(SeekFrom::Start(0))?;
    let header = read_wal_header(wal)?;
    if header.page_size != main_page_size {
        return Err(WalError::Corrupt(
            "state database WAL page size does not match the database",
        ));
    }

    let frame_bytes = WAL_FRAME_HEADER_BYTES + u64::from(header.page_size);
    let maximum_pages = u32::try_from(MAX_DATABASE_BYTES / u64::from(header.page_size))
        .expect("database inspection limit fits in u32 pages");
    let mut checksum = header.checksum;
    let mut frame_index = 0_u64;
    let mut last_committed_frame = 0_u64;
    let mut final_pages = main_pages;
    let mut frame_offset = WAL_HEADER_BYTES;
    let mut scratch = [0_u8; 8192];

    while wal_len.saturating_sub(frame_offset) >= frame_bytes {
        wal.seek(SeekFrom::Start(frame_offset))?;
        let mut frame_header = [0_u8; WAL_FRAME_HEADER_BYTES as usize];
        wal.read_exact(&mut frame_header)?;
        if frame_header[8..16] != header.salt {
            break;
        }
        let page_number = u32::from_be_bytes(frame_header[..4].try_into().unwrap());
        let committed_pages = u32::from_be_bytes(frame_header[4..8].try_into().unwrap());
        if page_number == 0 || page_number > maximum_pages {
            return Err(WalError::Limit(
                "state database WAL contains an invalid page number",
            ));
        }
        if committed_pages > maximum_pages {
            return Err(WalError::Limit(
                "state database WAL commit exceeds the inspection limit",
            ));
        }

        let mut candidate = checksum;
        update_checksum(&frame_header[..8], header.checksum_order, &mut candidate);
        let mut remaining = usize::try_from(header.page_size).unwrap();
        while remaining > 0 {
            let length = remaining.min(scratch.len());
            wal.read_exact(&mut scratch[..length])?;
            update_checksum(&scratch[..length], header.checksum_order, &mut candidate);
            remaining -= length;
        }
        let stored = [
            u32::from_be_bytes(frame_header[16..20].try_into().unwrap()),
            u32::from_be_bytes(frame_header[20..24].try_into().unwrap()),
        ];
        if candidate != stored {
            if committed_pages != 0
                || tail_contains_commit(
                    wal,
                    frame_offset + frame_bytes,
                    wal_len,
                    frame_bytes,
                    header.salt,
                )?
            {
                return Err(WalError::Corrupt(
                    "state database WAL contains a damaged committed frame",
                ));
            }
            break;
        }

        checksum = candidate;
        frame_index += 1;
        if committed_pages != 0 {
            if page_number > committed_pages {
                return Err(WalError::Corrupt(
                    "state database WAL commit references a page beyond the database",
                ));
            }
            last_committed_frame = frame_index;
            final_pages = committed_pages;
        }
        frame_offset = frame_offset
            .checked_add(frame_bytes)
            .ok_or(WalError::Corrupt("state database WAL offset overflowed"))?;
    }

    Ok(WalPlan {
        header,
        last_committed_frame,
        final_pages,
    })
}

fn index_wal<R: Read + Seek>(
    wal: &mut R,
    wal_len: u64,
    expected: WalPlan,
    main_pages: u32,
) -> Result<(Vec<u64>, u32), WalError> {
    let observed = analyze_wal(wal, wal_len, expected.header.page_size, main_pages)?;
    if observed != expected {
        return Err(WalError::Corrupt(
            "state database WAL changed during inspection",
        ));
    }

    let frame_bytes = WAL_FRAME_HEADER_BYTES + u64::from(expected.header.page_size);
    let page_slots = usize::try_from(expected.final_pages)
        .map_err(|_| WalError::Limit("state database page index exceeds the inspection limit"))?
        .checked_add(1)
        .ok_or(WalError::Limit(
            "state database page index exceeds the inspection limit",
        ))?;
    let mut pages = Vec::new();
    pages
        .try_reserve_exact(page_slots)
        .map_err(|_| WalError::Limit("unable to allocate the state database page index"))?;
    pages.resize(page_slots, 0_u64);
    let mut main_page_limit = main_pages;
    for frame_index in 0..expected.last_committed_frame {
        let frame_offset = WAL_HEADER_BYTES
            .checked_add(
                frame_index
                    .checked_mul(frame_bytes)
                    .ok_or(WalError::Corrupt("state database WAL offset overflowed"))?,
            )
            .ok_or(WalError::Corrupt("state database WAL offset overflowed"))?;
        wal.seek(SeekFrom::Start(frame_offset))?;
        let mut frame_header = [0_u8; WAL_FRAME_HEADER_BYTES as usize];
        wal.read_exact(&mut frame_header)?;
        let page_number = u32::from_be_bytes(frame_header[..4].try_into().unwrap());
        let committed_pages = u32::from_be_bytes(frame_header[4..8].try_into().unwrap());
        if let Some(page) = pages.get_mut(page_number as usize) {
            *page = frame_offset + WAL_FRAME_HEADER_BYTES;
        }
        if committed_pages != 0 {
            let first_removed = usize::try_from(committed_pages)
                .unwrap()
                .saturating_add(1)
                .min(pages.len());
            pages[first_removed..].fill(0);
            main_page_limit = main_page_limit.min(committed_pages);
        }
    }
    Ok((pages, main_page_limit))
}

fn read_wal_header<R: Read>(wal: &mut R) -> Result<WalHeader, WalError> {
    let mut bytes = [0_u8; WAL_HEADER_BYTES as usize];
    wal.read_exact(&mut bytes)?;
    let magic = u32::from_be_bytes(bytes[..4].try_into().unwrap());
    let checksum_order = match magic {
        WAL_MAGIC => ChecksumOrder::LittleEndian,
        value if value == WAL_MAGIC | 1 => ChecksumOrder::BigEndian,
        _ => return Err(WalError::Corrupt("state database WAL magic is invalid")),
    };
    if u32::from_be_bytes(bytes[4..8].try_into().unwrap()) != WAL_FORMAT_VERSION {
        return Err(WalError::Corrupt(
            "state database WAL format version is unsupported",
        ));
    }
    let page_size = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(WalError::Corrupt("state database WAL page size is invalid"));
    }
    let mut checksum = [0_u32; 2];
    update_checksum(&bytes[..24], checksum_order, &mut checksum);
    let stored = [
        u32::from_be_bytes(bytes[24..28].try_into().unwrap()),
        u32::from_be_bytes(bytes[28..32].try_into().unwrap()),
    ];
    if checksum != stored {
        return Err(WalError::Corrupt(
            "state database WAL header checksum is invalid",
        ));
    }
    Ok(WalHeader {
        page_size,
        salt: bytes[16..24].try_into().unwrap(),
        checksum_order,
        checksum,
    })
}

fn tail_contains_commit<R: Read + Seek>(
    wal: &mut R,
    mut offset: u64,
    wal_len: u64,
    frame_bytes: u64,
    salt: [u8; 8],
) -> Result<bool, WalError> {
    while wal_len.saturating_sub(offset) >= frame_bytes {
        wal.seek(SeekFrom::Start(offset))?;
        let mut header = [0_u8; WAL_FRAME_HEADER_BYTES as usize];
        wal.read_exact(&mut header)?;
        if header[8..16] != salt {
            return Ok(false);
        }
        if u32::from_be_bytes(header[4..8].try_into().unwrap()) != 0 {
            return Ok(true);
        }
        offset = offset
            .checked_add(frame_bytes)
            .ok_or(WalError::Corrupt("state database WAL offset overflowed"))?;
    }
    Ok(false)
}

fn update_checksum(bytes: &[u8], order: ChecksumOrder, checksum: &mut [u32; 2]) {
    debug_assert!(!bytes.is_empty() && bytes.len().is_multiple_of(8));
    for words in bytes.chunks_exact(8) {
        let first = match order {
            ChecksumOrder::LittleEndian => u32::from_le_bytes(words[..4].try_into().unwrap()),
            ChecksumOrder::BigEndian => u32::from_be_bytes(words[..4].try_into().unwrap()),
        };
        let second = match order {
            ChecksumOrder::LittleEndian => u32::from_le_bytes(words[4..].try_into().unwrap()),
            ChecksumOrder::BigEndian => u32::from_be_bytes(words[4..].try_into().unwrap()),
        };
        checksum[0] = checksum[0].wrapping_add(first).wrapping_add(checksum[1]);
        checksum[1] = checksum[1].wrapping_add(second).wrapping_add(checksum[0]);
    }
}

struct ReplayedDatabase {
    database: File,
    wal: File,
    wal_pages: Vec<u64>,
    main_page_limit: u32,
    main_len: u64,
    page_size: u64,
    position: u64,
}

impl Read for ReplayedDatabase {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        while written < buffer.len() {
            let page_index = self.position / self.page_size;
            let page_number = u32::try_from(page_index + 1)
                .map_err(|_| io::Error::other("replayed database page number overflowed"))?;
            let within_page = self.position % self.page_size;
            let remaining_page = usize::try_from(self.page_size - within_page).unwrap();
            let length = remaining_page.min(buffer.len() - written);
            let target = &mut buffer[written..written + length];
            let frame_offset = self
                .wal_pages
                .get(page_number as usize)
                .copied()
                .unwrap_or(0);
            if frame_offset != 0 {
                self.wal.seek(SeekFrom::Start(frame_offset + within_page))?;
                self.wal.read_exact(target)?;
            } else if page_number <= self.main_page_limit && self.position < self.main_len {
                self.database.seek(SeekFrom::Start(self.position))?;
                self.database.read_exact(target)?;
            } else {
                target.fill(0);
            }
            if self.position < 20 && self.position + u64::try_from(length).unwrap() > 18 {
                for header_offset in [18_u64, 19] {
                    if (self.position..self.position + u64::try_from(length).unwrap())
                        .contains(&header_offset)
                    {
                        target[usize::try_from(header_offset - self.position).unwrap()] = 1;
                    }
                }
            }
            self.position += u64::try_from(length).unwrap();
            written += length;
        }
        Ok(written)
    }
}

fn io_error(source: io::Error) -> StorageError {
    StorageError::Io { source }
}

fn database_error(source: rusqlite::Error) -> StorageError {
    StorageError::StateDatabase { source }
}

fn corrupt(message: &'static str) -> StorageError {
    StorageError::StateDatabaseCorrupt {
        message: message.to_owned(),
    }
}

fn resource_limit(message: &'static str) -> StorageError {
    io_error(io::Error::new(io::ErrorKind::OutOfMemory, message))
}

fn storage_error(error: WalError) -> StorageError {
    match error {
        WalError::Io(source) => io_error(source),
        WalError::Corrupt(message) => corrupt(message),
        WalError::Limit(message) => resource_limit(message),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const PAGE_SIZE: u32 = 512;
    const SALT: [u8; 8] = *b"12345678";

    #[test]
    fn analyzes_the_last_complete_commit() {
        let wal = test_wal(&[
            TestFrame::valid(1, 0),
            TestFrame::valid(2, 2),
            TestFrame::valid(2, 0),
            TestFrame::valid(3, 3),
        ]);

        let plan = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 1).unwrap();

        assert_eq!(plan.last_committed_frame, 4);
        assert_eq!(plan.final_pages, 3);
    }

    #[test]
    fn ignores_an_uncommitted_or_truncated_tail() {
        let mut wal = test_wal(&[
            TestFrame::valid(1, 1),
            TestFrame::valid(2, 0),
            TestFrame::valid(3, 0),
        ]);
        wal.truncate(wal.len() - 17);

        let plan = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 1).unwrap();

        assert_eq!(plan.last_committed_frame, 1);
        assert_eq!(plan.final_pages, 1);
    }

    #[test]
    fn keeps_the_main_database_when_the_wal_has_no_commit() {
        let wal = test_wal(&[TestFrame::valid(1, 0)]);

        let plan = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 7).unwrap();

        assert_eq!(plan.last_committed_frame, 0);
        assert_eq!(plan.final_pages, 7);
    }

    #[test]
    fn stops_at_a_stale_salt_even_if_the_tail_marks_a_commit() {
        let wal = test_wal(&[TestFrame::valid(1, 1), TestFrame::stale(2, 2)]);

        let plan = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 1).unwrap();

        assert_eq!(plan.last_committed_frame, 1);
        assert_eq!(plan.final_pages, 1);
    }

    #[test]
    fn rejects_a_damaged_commit_frame() {
        let wal = test_wal(&[TestFrame::valid(1, 0), TestFrame::damaged(2, 2)]);

        let result = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 1);

        assert!(matches!(
            result,
            Err(WalError::Corrupt(
                "state database WAL contains a damaged committed frame"
            ))
        ));
    }

    #[test]
    fn rejects_a_damaged_frame_that_a_later_frame_commits() {
        let wal = test_wal(&[TestFrame::damaged(1, 0), TestFrame::valid(2, 2)]);

        let result = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 1);

        assert!(matches!(
            result,
            Err(WalError::Corrupt(
                "state database WAL contains a damaged committed frame"
            ))
        ));
    }

    #[test]
    fn rejects_a_page_number_beyond_the_database_limit() {
        let maximum_pages = u32::try_from(MAX_DATABASE_BYTES / u64::from(PAGE_SIZE)).unwrap();
        let wal = test_wal(&[TestFrame::valid(maximum_pages + 1, 0)]);

        let result = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 1);

        assert!(matches!(
            result,
            Err(WalError::Limit(
                "state database WAL contains an invalid page number"
            ))
        ));
    }

    #[test]
    fn rejects_a_commit_beyond_the_database_limit() {
        let maximum_pages = u32::try_from(MAX_DATABASE_BYTES / u64::from(PAGE_SIZE)).unwrap();
        let wal = test_wal(&[TestFrame::valid(1, maximum_pages + 1)]);

        let result = analyze_wal(&mut Cursor::new(&wal), wal.len() as u64, PAGE_SIZE, 1);

        assert!(matches!(
            result,
            Err(WalError::Limit(
                "state database WAL commit exceeds the inspection limit"
            ))
        ));
    }

    #[derive(Clone, Copy)]
    struct TestFrame {
        page_number: u32,
        committed_pages: u32,
        salt: [u8; 8],
        damaged: bool,
    }

    impl TestFrame {
        fn valid(page_number: u32, committed_pages: u32) -> Self {
            Self {
                page_number,
                committed_pages,
                salt: SALT,
                damaged: false,
            }
        }

        fn stale(page_number: u32, committed_pages: u32) -> Self {
            Self {
                salt: *b"staleslt",
                ..Self::valid(page_number, committed_pages)
            }
        }

        fn damaged(page_number: u32, committed_pages: u32) -> Self {
            Self {
                damaged: true,
                ..Self::valid(page_number, committed_pages)
            }
        }
    }

    fn test_wal(frames: &[TestFrame]) -> Vec<u8> {
        let mut wal = vec![0_u8; WAL_HEADER_BYTES as usize];
        wal[..4].copy_from_slice(&(WAL_MAGIC | 1).to_be_bytes());
        wal[4..8].copy_from_slice(&WAL_FORMAT_VERSION.to_be_bytes());
        wal[8..12].copy_from_slice(&PAGE_SIZE.to_be_bytes());
        wal[16..24].copy_from_slice(&SALT);
        let mut checksum = [0_u32; 2];
        update_checksum(&wal[..24], ChecksumOrder::BigEndian, &mut checksum);
        wal[24..28].copy_from_slice(&checksum[0].to_be_bytes());
        wal[28..32].copy_from_slice(&checksum[1].to_be_bytes());

        for (index, frame) in frames.iter().enumerate() {
            let mut header = [0_u8; WAL_FRAME_HEADER_BYTES as usize];
            header[..4].copy_from_slice(&frame.page_number.to_be_bytes());
            header[4..8].copy_from_slice(&frame.committed_pages.to_be_bytes());
            header[8..16].copy_from_slice(&frame.salt);
            let page = vec![u8::try_from(index + 1).unwrap(); PAGE_SIZE as usize];
            let mut candidate = checksum;
            update_checksum(&header[..8], ChecksumOrder::BigEndian, &mut candidate);
            update_checksum(&page, ChecksumOrder::BigEndian, &mut candidate);
            header[16..20].copy_from_slice(&candidate[0].to_be_bytes());
            header[20..24].copy_from_slice(&candidate[1].to_be_bytes());
            if frame.damaged {
                header[20] ^= 1;
            } else if frame.salt == SALT {
                checksum = candidate;
            }
            wal.extend_from_slice(&header);
            wal.extend_from_slice(&page);
        }
        wal
    }
}
