//! A minimal Parquet writer for flat schemas of OPTIONAL `INT32`, `INT64` and UTF-8 columns.
//!
//! Every column chunk is encoded independently (one Snappy v1 data page, plus a dictionary page
//! for strings), so chunks can be produced on any thread and in any order and then assembled.
//! Pages are written in the Snappy raw format as a single literal: the codec is SNAPPY and every
//! reader decodes it, but no time goes into searching for matches in data that the encodings below
//! have already packed (Snappy saved 1-30% on these pages at a third of the encoding time). Integers use `DELTA_BINARY_PACKED`: timestamps and ids advance by small steps,
//! so a row costs a few bits, and nothing has to be hashed. Strings come from small fixed
//! vocabularies and are dictionary-encoded with bit-packed indices. Definition levels are one RLE
//! run when a column has no nulls, bit-packed otherwise.
//!
//! The footer is written with the Thrift compact protocol, mirroring parquet-rs's field set
//! (statistics omitted, `ColumnChunk.file_offset = 0`).

/// The four bytes a Parquet file starts and ends with.
pub const MAGIC: &[u8; 4] = b"PAR1";

// Thrift enum values (parquet.thrift).
const TYPE_INT32: i32 = 1;
const TYPE_INT64: i32 = 2;
const TYPE_BYTE_ARRAY: i32 = 6;
const ENC_PLAIN: i32 = 0;
const ENC_RLE: i32 = 3;
const ENC_DELTA_BINARY_PACKED: i32 = 5;
const ENC_RLE_DICTIONARY: i32 = 8;
const PAGE_DATA: i32 = 0;
const PAGE_DICTIONARY: i32 = 2;
const CODEC_SNAPPY: i32 = 1;
const REPETITION_OPTIONAL: i32 = 1;
const CONVERTED_UTF8: i32 = 0;

// ---------------------------------------------------------------------------------- varints

#[inline(always)]
fn uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline(always)]
fn zigzag64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline(always)]
fn zigzag32(v: i32) -> u64 {
    ((v << 1) ^ (v >> 31)) as u32 as u64
}

// ---------------------------------------------------------------------------------- thrift

/// Thrift compact-protocol struct writer. Nested structs keep their own last-field-id.
struct Thrift<'a> {
    out: &'a mut Vec<u8>,
    last: Vec<i16>,
}

const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

impl<'a> Thrift<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Thrift { out, last: vec![0] }
    }

    fn field(&mut self, ty: u8, id: i16) {
        let last = self.last.last_mut().expect("inside a struct");
        let delta = id - *last;
        if (1..=15).contains(&delta) {
            self.out.push(((delta as u8) << 4) | ty);
        } else {
            self.out.push(ty);
            uvarint(self.out, zigzag32(id as i32));
        }
        *last = id;
    }

    fn i32(&mut self, id: i16, v: i32) {
        self.field(T_I32, id);
        uvarint(self.out, zigzag32(v));
    }

    fn i64(&mut self, id: i16, v: i64) {
        self.field(T_I64, id);
        uvarint(self.out, zigzag64(v));
    }

    fn binary(&mut self, id: i16, v: &[u8]) {
        self.field(T_BINARY, id);
        uvarint(self.out, v.len() as u64);
        self.out.extend_from_slice(v);
    }

    fn list(&mut self, id: i16, elem: u8, n: usize) {
        self.field(T_LIST, id);
        if n < 15 {
            self.out.push(((n as u8) << 4) | elem);
        } else {
            self.out.push(0xF0 | elem);
            uvarint(self.out, n as u64);
        }
    }

    fn elem_i32(&mut self, v: i32) {
        uvarint(self.out, zigzag32(v));
    }

    fn elem_binary(&mut self, v: &[u8]) {
        uvarint(self.out, v.len() as u64);
        self.out.extend_from_slice(v);
    }

    /// Open a struct-typed field (`id = Some`) or a struct list element (`None`).
    fn begin(&mut self, id: Option<i16>) {
        if let Some(id) = id {
            self.field(T_STRUCT, id);
        }
        self.last.push(0);
    }

    fn end(&mut self) {
        self.out.push(0);
        self.last.pop();
    }
}

// ---------------------------------------------------------------------------------- bit packing

/// Append `values` bit-packed LSB-first at `width` bits (0..=64). `values.len()` must make a whole
/// number of bytes (`len * width % 8 == 0`).
#[inline]
fn bitpack(out: &mut Vec<u8>, values: &[u64], width: u32) {
    if width == 0 {
        return;
    }
    let mut acc: u128 = 0;
    let mut bits = 0u32;
    for &v in values {
        acc |= (v as u128) << bits;
        bits += width;
        if bits >= 64 {
            out.extend_from_slice(&(acc as u64).to_le_bytes());
            acc >>= 64;
            bits -= 64;
        }
    }
    debug_assert_eq!(bits % 8, 0);
    let tail = (bits / 8) as usize;
    out.extend_from_slice(&(acc as u64).to_le_bytes()[..tail]);
}

#[inline(always)]
fn width_of(max: u64) -> u32 {
    64 - max.leading_zeros()
}

/// Append 32 values bit-packed LSB-first at the constant width `W` (the values fit in `W` bits):
/// `4 * W` bytes. The loop unrolls, so every shift and flush point is known at compile time.
#[inline(always)]
fn pack32<const W: usize>(out: &mut Vec<u8>, values: &[u64; 32]) {
    let len = 4 * W;
    out.reserve(len + 8);
    let base = out.len();
    // SAFETY: `reserve` leaves at least `len + 8` writable bytes after `base`; every store below
    // lands inside them, and only the first `len` become part of `out`.
    unsafe {
        let dst = out.as_mut_ptr().add(base);
        let mut acc = 0u64;
        let mut filled = 0usize;
        let mut written = 0usize;
        for &v in values {
            acc |= v << filled;
            filled += W;
            if filled >= 64 {
                dst.add(written).cast::<u64>().write_unaligned(acc.to_le());
                written += 8;
                filled -= 64;
                acc = if filled == 0 { 0 } else { v >> (W - filled) };
            }
        }
        if filled > 0 {
            dst.add(written).cast::<u64>().write_unaligned(acc.to_le());
        }
        out.set_len(base + len);
    }
}

/// [`bitpack`] for exactly 32 values (one `DELTA_BINARY_PACKED` miniblock), by width.
fn bitpack32(out: &mut Vec<u8>, values: &[u64; 32], width: u32) {
    macro_rules! widths {
        ($($w:literal)*) => {
            match width {
                0 => {}
                $($w => pack32::<$w>(out, values),)*
                _ => unreachable!("bit width {width} > 64"),
            }
        };
    }
    widths!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32
            33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64);
}

/// RLE/bit-packed hybrid for definition levels (`max_def_level = 1`), with the v1 4-byte length.
fn def_levels(out: &mut Vec<u8>, present: Option<&[bool]>, n: usize) {
    let len_at = out.len();
    out.extend_from_slice(&[0; 4]);
    match present {
        None => {
            // One RLE run of `n` ones.
            uvarint(out, (n as u64) << 1);
            out.push(1);
        }
        Some(present) => {
            let groups = n.div_ceil(8);
            uvarint(out, ((groups as u64) << 1) | 1);
            for chunk in present.chunks(8) {
                let mut byte = 0u8;
                for (i, &p) in chunk.iter().enumerate() {
                    byte |= (p as u8) << i;
                }
                out.push(byte);
            }
        }
    }
    let len = (out.len() - len_at - 4) as u32;
    out[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
}

/// `DELTA_BINARY_PACKED` (block 128, 4 miniblocks of 32) over `values`, in `bits`-bit wrapping
/// arithmetic (32 for INT32 columns, 64 for INT64) exactly as readers reconstruct it.
fn delta_binary_packed(out: &mut Vec<u8>, values: &[i64], bits: u32) {
    if bits == 32 {
        delta_binary_packed_as::<32>(out, values);
    } else {
        delta_binary_packed_as::<64>(out, values);
    }
}

fn delta_binary_packed_as<const BITS: u32>(out: &mut Vec<u8>, values: &[i64]) {
    const BLOCK: usize = 128;
    const MINIBLOCKS: usize = 4;
    const MINI: usize = BLOCK / MINIBLOCKS;
    #[inline(always)]
    fn wrap<const BITS: u32>(v: i64) -> i64 {
        if BITS == 32 { v as i32 as i64 } else { v }
    }
    #[inline(always)]
    fn zigzag<const BITS: u32>(v: i64) -> u64 {
        if BITS == 32 { zigzag32(v as i32) } else { zigzag64(v) }
    }
    let mask = if BITS == 32 { u32::MAX as u64 } else { u64::MAX };
    uvarint(out, BLOCK as u64);
    uvarint(out, MINIBLOCKS as u64);
    uvarint(out, values.len() as u64);
    let Some(&first) = values.first() else {
        uvarint(out, 0);
        return;
    };
    uvarint(out, zigzag::<BITS>(first));
    let mut deltas = [0i64; BLOCK];
    let mut packed = [0u64; MINI];
    let mut prev = first;
    let mut rest = &values[1..];
    while !rest.is_empty() {
        let n = rest.len().min(BLOCK);
        let block = &rest[..n];
        deltas[0] = wrap::<BITS>(block[0].wrapping_sub(prev));
        for i in 1..n {
            deltas[i] = wrap::<BITS>(block[i].wrapping_sub(block[i - 1]));
        }
        prev = block[n - 1];
        let min = deltas[..n].iter().copied().min().expect("non-empty block");
        rest = &rest[n..];
        uvarint(out, zigzag::<BITS>(min));
        let used = n.div_ceil(MINI);
        let mut widths = [0u8; MINIBLOCKS];
        let width_at = out.len();
        out.extend_from_slice(&widths);
        for m in 0..used {
            let span = &deltas[m * MINI..(m * MINI + MINI).min(n)];
            let mut max = 0u64;
            for (p, &d) in packed.iter_mut().zip(span) {
                *p = (d.wrapping_sub(min) as u64) & mask;
                max |= *p;
            }
            packed[span.len()..].fill(0); // the last miniblock is padded to 32 values
            // The OR of the values needs exactly the bits of their maximum.
            widths[m] = width_of(max) as u8;
            bitpack32(out, &packed, widths[m] as u32);
        }
        out[width_at..width_at + MINIBLOCKS].copy_from_slice(&widths);
    }
}

// ---------------------------------------------------------------------------------- pages

/// Reusable per-thread scratch space for encoding column chunks.
pub struct Scratch {
    page: Vec<u8>,
    ints: Vec<i64>,
    present: Vec<bool>,
    indices: Vec<u64>,
}

impl Default for Scratch {
    fn default() -> Self {
        Scratch {
            page: Vec::with_capacity(1 << 20),
            ints: Vec::with_capacity(1 << 16),
            present: Vec::with_capacity(1 << 16),
            indices: Vec::with_capacity(1 << 16),
        }
    }
}

/// One encoded column chunk plus the metadata the footer needs; offsets are chunk-relative.
pub struct ColumnChunk {
    pub bytes: Vec<u8>,
    /// Bytes already handed out by [`ColumnChunk::take_bytes`] (written ahead of the footer).
    taken: usize,
    physical: i32,
    encodings: &'static [i32],
    num_values: i64,
    uncompressed: i64,
    data_page_offset: i64,
    dictionary_page_offset: Option<i64>,
    /// A dictionary column's bit-packed indices: where they start in `bytes`, and their width.
    dict_indices: Option<(usize, u32)>,
}

impl ColumnChunk {
    pub fn num_values(&self) -> i64 {
        self.num_values
    }

    /// Hand over the encoded bytes, to be written before the footer; the footer metadata stays.
    pub fn take_bytes(&mut self) -> Vec<u8> {
        self.taken += self.bytes.len();
        std::mem::take(&mut self.bytes)
    }

    /// Set row `row`'s index in a column chunk made by [`Scratch::dict_column`], in place: the
    /// indices are bit-packed at a fixed width, so the bytes keep their length and every other
    /// row's index. The bytes must not have been taken.
    pub fn set_dict_index(&mut self, row: usize, index: u8) {
        let (at, width) = self.dict_indices.expect("a dictionary column");
        debug_assert!(self.taken == 0 && width <= 8 && u32::from(index) < 1 << width);
        let bit = row * width as usize;
        let byte = at + bit / 8;
        let shift = bit % 8;
        let mask = ((1u16 << width) - 1) << shift;
        let spans_two = shift + width as usize > 8;
        let mut word = u16::from(self.bytes[byte]);
        if spans_two {
            word |= u16::from(self.bytes[byte + 1]) << 8;
        }
        word = (word & !mask) | (u16::from(index) << shift);
        self.bytes[byte] = word as u8;
        if spans_two {
            self.bytes[byte + 1] = (word >> 8) as u8;
        }
    }

    /// The chunk's size in the file, whether or not its bytes were taken.
    fn compressed_len(&self) -> usize {
        self.taken + self.bytes.len()
    }
}

/// The Snappy raw-format prefix of `len` bytes stored as one literal: the uncompressed length
/// (varint), then a literal tag with `len - 1` (in the tag for up to 60 bytes, else 1-4 bytes).
fn snappy_literal_prefix(len: usize) -> ([u8; 16], usize) {
    let mut prefix = [0u8; 16];
    let mut n = 0;
    let mut v = len as u64;
    while v >= 0x80 {
        prefix[n] = (v as u8) | 0x80;
        v >>= 7;
        n += 1;
    }
    prefix[n] = v as u8;
    n += 1;
    if len > 0 {
        let m = (len - 1) as u32;
        if m < 60 {
            prefix[n] = (m as u8) << 2;
            n += 1;
        } else {
            let bytes = 4 - m.leading_zeros() as usize / 8;
            prefix[n] = ((59 + bytes) as u8) << 2;
            prefix[n + 1..n + 1 + bytes].copy_from_slice(&m.to_le_bytes()[..bytes]);
            n += 1 + bytes;
        }
    }
    (prefix, n)
}

/// Append a Snappy page (header + body as one literal) to `chunk`; returns uncompressed bytes counted.
fn emit_page(chunk: &mut Vec<u8>, body: &[u8], page_type: i32, header_body: impl FnOnce(&mut Thrift<'_>)) -> i64 {
    let (prefix, prefix_len) = snappy_literal_prefix(body.len());
    let mut header = Vec::with_capacity(32);
    {
        let mut t = Thrift::new(&mut header);
        t.i32(1, page_type);
        t.i32(2, body.len() as i32);
        t.i32(3, (prefix_len + body.len()) as i32);
        header_body(&mut t);
        t.end();
    }
    chunk.reserve(header.len() + prefix_len + body.len());
    chunk.extend_from_slice(&header);
    chunk.extend_from_slice(&prefix[..prefix_len]);
    chunk.extend_from_slice(body);
    (header.len() + body.len()) as i64
}

impl Scratch {
    /// An integer column (values in `self.ints`, one per non-null row; `present` marks rows when
    /// the column has nulls) of `n_rows` rows.
    fn int_chunk(&mut self, physical: i32, n_rows: usize, nullable: bool) -> ColumnChunk {
        self.page.clear();
        def_levels(&mut self.page, nullable.then_some(&self.present[..]), n_rows);
        let bits = if physical == TYPE_INT32 { 32 } else { 64 };
        let encoding = if self.ints.is_empty() {
            ENC_PLAIN // an all-null page: no values at all
        } else {
            delta_binary_packed(&mut self.page, &self.ints, bits);
            ENC_DELTA_BINARY_PACKED
        };
        let mut bytes = Vec::with_capacity(self.page.len() + 64);
        let uncompressed = emit_page(&mut bytes, &self.page, PAGE_DATA, |t| {
            t.begin(Some(5));
            t.i32(1, n_rows as i32);
            t.i32(2, encoding);
            t.i32(3, ENC_RLE);
            t.i32(4, ENC_RLE);
            t.end();
        });
        ColumnChunk {
            bytes,
            taken: 0,
            physical,
            encodings: if encoding == ENC_PLAIN { &[ENC_PLAIN, ENC_RLE] } else { &[ENC_DELTA_BINARY_PACKED, ENC_RLE] },
            num_values: n_rows as i64,
            uncompressed,
            data_page_offset: 0,
            dictionary_page_offset: None,
            dict_indices: None,
        }
    }

    /// A non-null INT64 column.
    pub fn i64_column(&mut self, values: impl Iterator<Item = i64>) -> ColumnChunk {
        self.ints.clear();
        self.ints.extend(values);
        let n = self.ints.len();
        self.int_chunk(TYPE_INT64, n, false)
    }

    /// A non-null INT32 column.
    pub fn i32_column(&mut self, values: impl Iterator<Item = i32>) -> ColumnChunk {
        self.ints.clear();
        self.ints.extend(values.map(i64::from));
        let n = self.ints.len();
        self.int_chunk(TYPE_INT32, n, false)
    }

    /// A nullable INT64 column; `None` rows are nulls.
    pub fn nullable_i64_column(&mut self, values: impl Iterator<Item = Option<i64>>) -> ColumnChunk {
        self.ints.clear();
        self.present.clear();
        for v in values {
            self.present.push(v.is_some());
            if let Some(v) = v {
                self.ints.push(v);
            }
        }
        let n = self.present.len();
        self.int_chunk(TYPE_INT64, n, true)
    }

    /// A non-null UTF-8 column drawn from `dictionary`; `indices` index into it.
    pub fn dict_column(&mut self, dictionary: &[&str], indices: impl Iterator<Item = u8>) -> ColumnChunk {
        self.indices.clear();
        self.indices.extend(indices.map(u64::from));
        let n = self.indices.len();
        let mut bytes = Vec::with_capacity(128 + n / 2);

        // Dictionary page: PLAIN byte arrays.
        self.page.clear();
        for s in dictionary {
            self.page.extend_from_slice(&(s.len() as u32).to_le_bytes());
            self.page.extend_from_slice(s.as_bytes());
        }
        let dict_uncompressed = emit_page(&mut bytes, &self.page, PAGE_DICTIONARY, |t| {
            t.begin(Some(7));
            t.i32(1, dictionary.len() as i32);
            t.i32(2, ENC_PLAIN);
            t.end();
        });
        let data_page_offset = bytes.len() as i64;

        // Data page: definition levels, then bit width + bit-packed indices.
        self.page.clear();
        def_levels(&mut self.page, None, n);
        let width = width_of(dictionary.len().saturating_sub(1) as u64).max(1);
        self.page.push(width as u8);
        let groups = n.div_ceil(8);
        uvarint(&mut self.page, ((groups as u64) << 1) | 1);
        self.indices.resize(groups * 8, 0);
        let indices_at = self.page.len();
        let whole = self.indices.len() / 32 * 32;
        for chunk in self.indices[..whole].chunks_exact(32) {
            bitpack32(&mut self.page, chunk.try_into().expect("32 values"), width);
        }
        bitpack(&mut self.page, &self.indices[whole..], width);
        let data_uncompressed = emit_page(&mut bytes, &self.page, PAGE_DATA, |t| {
            t.begin(Some(5));
            t.i32(1, n as i32);
            t.i32(2, ENC_RLE_DICTIONARY);
            t.i32(3, ENC_RLE);
            t.i32(4, ENC_RLE);
            t.end();
        });
        // The page body ends the chunk (stored as one Snappy literal).
        let body_at = bytes.len() - self.page.len();
        ColumnChunk {
            bytes,
            taken: 0,
            physical: TYPE_BYTE_ARRAY,
            encodings: &[ENC_PLAIN, ENC_RLE_DICTIONARY, ENC_RLE],
            num_values: n as i64,
            uncompressed: dict_uncompressed + data_uncompressed,
            data_page_offset,
            dictionary_page_offset: Some(0),
            dict_indices: Some((body_at + indices_at, width)),
        }
    }
}

// ---------------------------------------------------------------------------------- files

#[derive(Clone, Copy)]
pub enum ColumnType {
    Int32,
    Int64,
    Utf8,
}

pub struct Schema {
    pub columns: &'static [(&'static str, ColumnType)],
    /// Key-value metadata (e.g. the `pandas` block).
    pub metadata: Vec<(String, String)>,
}

/// One row group: its column chunks in schema order and its row count.
pub struct RowGroup {
    pub columns: Vec<ColumnChunk>,
    pub num_rows: usize,
}

/// Concatenate row groups into a complete Parquet file.
pub fn assemble(schema: &Schema, groups: Vec<RowGroup>) -> Vec<u8> {
    assemble_pieces(schema, groups).to_bytes()
}

/// [`assemble`] with the column chunks of column `deferred` after all the others (see
/// [`assemble_pieces_deferred`]).
pub fn assemble_deferred(schema: &Schema, groups: Vec<RowGroup>, deferred: usize) -> Vec<u8> {
    assemble_pieces_deferred(schema, groups, deferred).to_bytes()
}

/// A Parquet file as the pieces it is made of, in order: the magic, every column chunk, then the
/// footer (metadata, its length and the magic). Writing or hashing the pieces in order gives the
/// file's bytes without first copying them into one buffer.
pub struct ParquetFile {
    /// Whether the leading magic is still to be written (not yet streamed out).
    magic: bool,
    chunks: Vec<Vec<u8>>,
    footer: Vec<u8>,
}

impl ParquetFile {
    pub fn pieces(&self) -> impl Iterator<Item = &[u8]> {
        self.magic
            .then_some(&MAGIC[..])
            .into_iter()
            .chain(self.chunks.iter().map(Vec::as_slice))
            .chain(std::iter::once(self.footer.as_slice()))
    }

    /// The rest of a file whose magic (when `magic_written`) and taken chunk bytes were already
    /// written in order: the untaken chunks and the footer.
    pub fn after_streamed(mut self, magic_written: bool) -> ParquetFile {
        self.magic &= !magic_written;
        self.chunks.retain(|c| !c.is_empty());
        self
    }

    pub fn len(&self) -> usize {
        self.pieces().map(<[u8]>::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        false // always holds at least the magic and the footer
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.len());
        for piece in self.pieces() {
            bytes.extend_from_slice(piece);
        }
        bytes
    }
}

/// Lay out `groups` as a Parquet file ([`ParquetFile`]), taking ownership of the chunk bytes.
pub fn assemble_pieces(schema: &Schema, groups: Vec<RowGroup>) -> ParquetFile {
    lay_out(schema, groups, None)
}

/// [`assemble_pieces`], with the chunks of column `deferred` placed after every other column
/// chunk, in row-group order, instead of inside their row groups. Everything before them can be
/// written (and hashed) before that column is known. Readers find each column chunk by the
/// offsets in the footer, as they do in any file.
pub fn assemble_pieces_deferred(schema: &Schema, groups: Vec<RowGroup>, deferred: usize) -> ParquetFile {
    lay_out(schema, groups, Some(deferred))
}

fn lay_out(schema: &Schema, groups: Vec<RowGroup>, deferred: Option<usize>) -> ParquetFile {
    let mut offset = MAGIC.len() as i64;
    let mut starts: Vec<Vec<i64>> = Vec::with_capacity(groups.len());
    for g in &groups {
        debug_assert_eq!(g.columns.len(), schema.columns.len());
        let mut s = Vec::with_capacity(g.columns.len());
        for (i, c) in g.columns.iter().enumerate() {
            s.push(offset);
            if Some(i) != deferred {
                offset += c.compressed_len() as i64;
            }
        }
        starts.push(s);
    }
    if let Some(d) = deferred {
        for (g, s) in groups.iter().zip(&mut starts) {
            s[d] = offset;
            offset += g.columns[d].compressed_len() as i64;
        }
    }

    let mut footer = Vec::with_capacity(1024 + groups.len() * schema.columns.len() * 64);
    {
        let mut t = Thrift::new(&mut footer);
        t.i32(1, 2); // version
        t.list(2, T_STRUCT, schema.columns.len() + 1);
        t.begin(None);
        t.binary(4, b"schema");
        t.i32(5, schema.columns.len() as i32);
        t.end();
        for (name, ty) in schema.columns {
            t.begin(None);
            let physical = match ty {
                ColumnType::Int32 => TYPE_INT32,
                ColumnType::Int64 => TYPE_INT64,
                ColumnType::Utf8 => TYPE_BYTE_ARRAY,
            };
            t.i32(1, physical);
            t.i32(3, REPETITION_OPTIONAL);
            t.binary(4, name.as_bytes());
            if matches!(ty, ColumnType::Utf8) {
                t.i32(6, CONVERTED_UTF8);
                t.begin(Some(10)); // LogicalType union
                t.begin(Some(1)); // STRING
                t.end();
                t.end();
            }
            t.end();
        }
        let num_rows: usize = groups.iter().map(|g| g.num_rows).sum();
        t.i64(3, num_rows as i64);
        t.list(4, T_STRUCT, groups.len());
        for (ordinal, (g, s)) in groups.iter().zip(&starts).enumerate() {
            t.begin(None);
            t.list(1, T_STRUCT, g.columns.len());
            let mut total_uncompressed = 0i64;
            let mut total_compressed = 0i64;
            for ((c, &start), (name, _)) in g.columns.iter().zip(s).zip(schema.columns) {
                total_uncompressed += c.uncompressed;
                total_compressed += c.compressed_len() as i64;
                t.begin(None);
                t.i64(2, 0); // file_offset: deprecated; 0 as parquet-rs writes
                t.begin(Some(3));
                t.i32(1, c.physical);
                t.list(2, T_I32, c.encodings.len());
                for &e in c.encodings {
                    t.elem_i32(e);
                }
                t.list(3, T_BINARY, 1);
                t.elem_binary(name.as_bytes());
                t.i32(4, CODEC_SNAPPY);
                t.i64(5, c.num_values);
                t.i64(6, c.uncompressed);
                t.i64(7, c.compressed_len() as i64);
                t.i64(9, start + c.data_page_offset);
                if let Some(d) = c.dictionary_page_offset {
                    t.i64(11, start + d);
                }
                t.end();
                t.end();
            }
            t.i64(2, total_uncompressed);
            t.i64(3, g.num_rows as i64);
            // The row group's first byte in the file (its first column's, unless deferred).
            t.i64(5, s.iter().copied().min().unwrap_or(offset));
            t.i64(6, total_compressed);
            if let Ok(ordinal) = i16::try_from(ordinal) {
                t.field(4, 7); // i16 ordinal
                uvarint(t.out, zigzag32(ordinal as i32));
            }
            t.end();
        }
        if !schema.metadata.is_empty() {
            t.list(5, T_STRUCT, schema.metadata.len());
            for (k, v) in &schema.metadata {
                t.begin(None);
                t.binary(1, k.as_bytes());
                t.binary(2, v.as_bytes());
                t.end();
            }
        }
        t.binary(6, concat!("fastsim version ", env!("CARGO_PKG_VERSION")).as_bytes());
        t.end();
    }
    let metadata_len = footer.len() as u32;
    footer.extend_from_slice(&metadata_len.to_le_bytes());
    footer.extend_from_slice(MAGIC);
    let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(groups.len() * schema.columns.len());
    let mut last = Vec::new();
    for g in groups {
        for (i, c) in g.columns.into_iter().enumerate() {
            if Some(i) == deferred { last.push(c.bytes) } else { chunks.push(c.bytes) }
        }
    }
    chunks.extend(last);
    ParquetFile { magic: true, chunks, footer }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference decoder for the bit layout, independent of the packer.
    fn unpack(bytes: &[u8], width: u32, n: usize) -> Vec<u64> {
        (0..n)
            .map(|i| {
                let mut v = 0u64;
                for b in 0..width as usize {
                    let bit = i * width as usize + b;
                    v |= (((bytes[bit / 8] >> (bit % 8)) & 1) as u64) << b;
                }
                v
            })
            .collect()
    }

    #[test]
    fn bitpack_round_trips_every_width() {
        let mut rng = crate::rng::Mt19937::new(7);
        for width in 0..=64u32 {
            let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
            let values: Vec<u64> = (0..64).map(|_| rng.next_u64() & mask).collect();
            let mut out = Vec::new();
            bitpack(&mut out, &values, width);
            assert_eq!(out.len(), 64 * width as usize / 8);
            if width > 0 {
                assert_eq!(unpack(&out, width, 64), values, "width {width}");
            }
        }
    }

    #[test]
    fn patched_dictionary_indices_match_encoding_the_patched_values() {
        let mut rng = crate::rng::Mt19937::new(11);
        for size in [1usize, 2, 3, 5, 9, 17, 200] {
            let dictionary: Vec<String> = (0..size).map(|i| format!("v{i}")).collect();
            let dictionary: Vec<&str> = dictionary.iter().map(String::as_str).collect();
            for n in [1usize, 7, 8, 31, 32, 33, 1000] {
                let mut values: Vec<u8> = (0..n).map(|_| (rng.next_u64() % size as u64) as u8).collect();
                let mut s = Scratch::default();
                let mut patched = s.dict_column(&dictionary, values.iter().copied());
                for _ in 0..n.div_ceil(3) {
                    let row = (rng.next_u64() % n as u64) as usize;
                    let index = (rng.next_u64() % size as u64) as u8;
                    values[row] = index;
                    patched.set_dict_index(row, index);
                }
                let direct = s.dict_column(&dictionary, values.iter().copied());
                assert_eq!(patched.bytes, direct.bytes, "dictionary {size}, {n} rows");
            }
        }
    }
}
