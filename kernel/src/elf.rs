//! Validation and mapping primitives for the root task's ELF image.
//!
//! This module deliberately does not depend on the scheduler, address-space objects, or the
//! Limine request globals.  P2.5 supplies the concrete [`SegmentMapper`] implementation; until
//! then the parser can validate a Limine module without giving it any authority.

use core::ops::Range;

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PAGE_SIZE: u64 = 4096;

/// A Limine module viewed as a bounded byte slice.
///
/// The caller must construct this slice from Limine's module address and size through the HHDM;
/// no address arithmetic is performed by this parser.
pub struct Module<'a> {
    /// Bytes handed over by Limine.
    pub bytes: &'a [u8],
}

/// A validated ELF load segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadSegment {
    /// Virtual address at which the segment starts.
    pub virtual_address: u64,
    /// Number of bytes present in the module.
    pub file_size: u64,
    /// Number of bytes mapped, including zero-filled BSS.
    pub memory_size: u64,
    /// File offset of the segment.
    pub file_offset: u64,
    /// ELF segment flags (PF_R, PF_W, PF_X).
    pub flags: u32,
    /// Required segment alignment.
    pub alignment: u64,
}

impl LoadSegment {
    /// Whether this segment may be executed.
    pub const fn executable(self) -> bool {
        self.flags & PF_X != 0
    }

    /// Whether this segment may be written.
    pub const fn writable(self) -> bool {
        self.flags & PF_W != 0
    }
}

/// Validated root-task image and its load segments.
pub struct Image<'a, 's> {
    bytes: &'a [u8],
    /// Entry point for the initial user thread.
    pub entry: u64,
    /// Validated load segments in program-header order.
    pub segments: &'s [LoadSegment],
}

/// Errors returned while validating a root-task ELF image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The module is too short for the ELF header.
    TruncatedHeader,
    /// The ELF identification bytes or class/data encoding is unsupported.
    BadIdentification,
    /// The ELF version, machine, or file type is unsupported.
    UnsupportedFormat,
    /// The program-header table is missing or outside the module.
    InvalidProgramHeaders,
    /// A program header is malformed or outside the module.
    InvalidSegment,
    /// A segment's arithmetic overflows or its file data exceeds the module.
    SegmentOutOfBounds,
    /// A segment requests writable and executable pages simultaneously.
    WritableExecutable,
    /// No PT_LOAD segment contains the entry point.
    EntryNotMapped,
    /// The image has more load segments than the fixed kernel scratch array.
    TooManySegments,
    /// The mapper rejected a segment.
    MappingFailed,
}

/// Scratch capacity used by [`validate`]. A normal root image should need only a handful.
pub const MAX_SEGMENTS: usize = 32;

/// Validates a Limine module and returns its entry point and load segments.
///
/// The returned image borrows the module and a caller-provided fixed-size segment array, so this
/// function performs no allocation. `ET_EXEC` and `ET_DYN` are accepted; PIE relocation is not
/// performed, so an `ET_DYN` image must already use its final virtual addresses.
pub fn validate<'a, 's>(
    module: Module<'a>,
    output: &'s mut [LoadSegment; MAX_SEGMENTS],
) -> Result<Image<'a, 's>, Error> {
    let bytes = module.bytes;
    if bytes.len() < 64 {
        return Err(Error::TruncatedHeader);
    }
    if &bytes[0..4] != ELF_MAGIC
        || bytes[4] != ELFCLASS64
        || bytes[5] != ELFDATA2LSB
        || bytes[6] != EV_CURRENT
    {
        return Err(Error::BadIdentification);
    }
    if u16le(bytes, 16)? != ET_EXEC && u16le(bytes, 16)? != ET_DYN
        || u16le(bytes, 18)? != EM_X86_64
        || u32le(bytes, 20)? != 1
    {
        return Err(Error::UnsupportedFormat);
    }
    let entry = u64le(bytes, 24)?;
    let phoff = u64le(bytes, 32)?;
    let phentsize = u16le(bytes, 54)? as u64;
    let phnum = u16le(bytes, 56)? as u64;
    if phentsize != 56 || phnum == 0 {
        return Err(Error::InvalidProgramHeaders);
    }
    let table_len = phentsize
        .checked_mul(phnum)
        .ok_or(Error::InvalidProgramHeaders)?;
    let table_end = phoff
        .checked_add(table_len)
        .ok_or(Error::InvalidProgramHeaders)?;
    if table_end > bytes.len() as u64 {
        return Err(Error::InvalidProgramHeaders);
    }

    let mut count = 0usize;
    for index in 0..phnum {
        let at = phoff + index * phentsize;
        if u32le(bytes, at)? != PT_LOAD {
            continue;
        }
        if count == MAX_SEGMENTS {
            return Err(Error::TooManySegments);
        }
        let file_offset = u64le(bytes, at + 8)?;
        let virtual_address = u64le(bytes, at + 16)?;
        let file_size = u64le(bytes, at + 32)?;
        let memory_size = u64le(bytes, at + 40)?;
        let flags = u32le(bytes, at + 4)?;
        let alignment = u64le(bytes, at + 48)?;
        if memory_size < file_size || file_offset % PAGE_SIZE != virtual_address % PAGE_SIZE {
            return Err(Error::InvalidSegment);
        }
        if alignment != 0 && (!alignment.is_power_of_two() || alignment < PAGE_SIZE) {
            return Err(Error::InvalidSegment);
        }
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(Error::WritableExecutable);
        }
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or(Error::SegmentOutOfBounds)?;
        let virtual_end = virtual_address
            .checked_add(memory_size)
            .ok_or(Error::SegmentOutOfBounds)?;
        if file_end > bytes.len() as u64 || virtual_end <= virtual_address {
            return Err(Error::SegmentOutOfBounds);
        }
        if output[..count].iter().any(|previous| {
            let previous_end = previous.virtual_address + previous.memory_size;
            virtual_address < previous_end && previous.virtual_address < virtual_end
        }) {
            return Err(Error::InvalidSegment);
        }
        output[count] = LoadSegment {
            virtual_address,
            file_size,
            memory_size,
            file_offset,
            flags,
            alignment,
        };
        count += 1;
    }
    if count == 0
        || !output[..count].iter().any(|segment| {
            segment.executable()
                && segment.virtual_address <= entry
                && entry < segment.virtual_address + segment.memory_size
        })
    {
        return Err(Error::EntryNotMapped);
    }
    Ok(Image {
        bytes,
        entry,
        segments: &output[..count],
    })
}

/// Operations supplied by the address-space implementation to materialize ELF segments.
pub trait SegmentMapper {
    /// Map zeroed, non-overlapping user pages covering `virtual_range` with ELF `flags`.
    fn map_zeroed(&mut self, virtual_range: Range<u64>, flags: u32) -> Result<(), Error>;
    /// Copy initialized bytes into a previously mapped user range.
    fn copy(&mut self, virtual_address: u64, bytes: &[u8]) -> Result<(), Error>;
}

impl<'a, 's> Image<'a, 's> {
    /// Maps every validated segment and copies its initialized data.
    ///
    /// Mapping is page-rounded while copying uses the exact ELF file range; bytes between
    /// `file_size` and `memory_size` remain zeroed by [`SegmentMapper::map_zeroed`].
    pub fn map_into<M: SegmentMapper>(&self, mapper: &mut M) -> Result<u64, Error> {
        for segment in self.segments {
            let start = segment.virtual_address & !(PAGE_SIZE - 1);
            let end = segment
                .virtual_address
                .checked_add(segment.memory_size)
                .and_then(|v| v.checked_add(PAGE_SIZE - 1))
                .map(|v| v & !(PAGE_SIZE - 1))
                .ok_or(Error::SegmentOutOfBounds)?;
            mapper
                .map_zeroed(start..end, segment.flags)
                .map_err(|_| Error::MappingFailed)?;
            let begin = segment.file_offset as usize;
            let end_file = begin
                .checked_add(segment.file_size as usize)
                .ok_or(Error::SegmentOutOfBounds)?;
            mapper
                .copy(segment.virtual_address, &self.bytes[begin..end_file])
                .map_err(|_| Error::MappingFailed)?;
        }
        Ok(self.entry)
    }
}

fn u16le(bytes: &[u8], offset: u64) -> Result<u16, Error> {
    let at = offset as usize;
    let end = at.checked_add(2).ok_or(Error::TruncatedHeader)?;
    let value = bytes.get(at..end).ok_or(Error::TruncatedHeader)?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn u32le(bytes: &[u8], offset: u64) -> Result<u32, Error> {
    let at = offset as usize;
    let end = at.checked_add(4).ok_or(Error::TruncatedHeader)?;
    let value = bytes.get(at..end).ok_or(Error::TruncatedHeader)?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn u64le(bytes: &[u8], offset: u64) -> Result<u64, Error> {
    let at = offset as usize;
    let end = at.checked_add(8).ok_or(Error::TruncatedHeader)?;
    let value = bytes.get(at..end).ok_or(Error::TruncatedHeader)?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}
