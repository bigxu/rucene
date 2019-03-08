mod index_options;

pub use self::index_options::*;

mod doc_values_type;

pub use self::doc_values_type::*;

mod index_writer;

pub use self::index_writer::*;

mod norm_values_writer;

pub use self::norm_values_writer::*;

mod numeric_doc_values;

pub use self::numeric_doc_values::*;

mod binary_doc_values;

pub use self::binary_doc_values::*;

mod sorted_numeric_doc_values;

pub use self::sorted_numeric_doc_values::*;

mod sorted_doc_values_term_iterator;

pub use self::sorted_doc_values_term_iterator::*;

mod sorted_set_doc_values_term_iterator;

pub use self::sorted_set_doc_values_term_iterator::*;

mod doc_values;

pub use self::doc_values::*;

mod doc_values_writer;

pub use self::doc_values_writer::*;

mod sorted_doc_values;

pub use self::sorted_doc_values::*;

mod singleton_sorted_numeric_doc_values;

pub use self::singleton_sorted_numeric_doc_values::*;

mod sorted_set_doc_values;

pub use self::sorted_set_doc_values::*;

mod singleton_sorted_set_doc_values;

pub use self::singleton_sorted_set_doc_values::*;

mod segment_doc_values;

pub use self::segment_doc_values::*;

mod segment_reader;

pub use self::segment_reader::*;

mod directory_reader;

pub use self::directory_reader::*;

mod segment;

pub use self::segment::*;

mod point_values;

pub use self::point_values::*;

pub mod field_info;

pub use self::field_info::*;

mod leaf_reader;

pub use self::leaf_reader::*;

mod term;

pub use self::term::TermState;
pub use self::term::*;

mod fieldable;

pub use self::fieldable::*;

mod index_lookup;

pub use self::index_lookup::*;

mod multi_fields;

pub use self::multi_fields::*;

mod multi_terms;

pub use self::multi_terms::*;

mod reader_slice;

pub use self::reader_slice::*;

mod stored_field_visitor;

pub use self::stored_field_visitor::*;

mod merge_state;

pub use self::merge_state::*;

mod point_values_writer;

pub use self::point_values_writer::*;

pub mod doc_id_merger;

mod bufferd_updates;
mod byte_slice_reader;
mod delete_policy;
mod doc_consumer;
mod doc_writer;
mod doc_writer_delete_queue;
mod doc_writer_flush_queue;
mod flush_control;
mod flush_policy;
mod index_commit;
mod index_file_deleter;
pub mod index_writer_config;
mod leaf_reader_wrapper;
pub mod merge_policy;
mod merge_rate_limiter;
pub mod merge_scheduler;
mod postings_array;
mod prefix_code_terms;
mod segment_merger;
mod sorter;
mod term_vector;
mod terms_hash;
mod terms_hash_per_field;
mod thread_doc_writer;

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::any::Any;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::result;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering as AtomicOrdering};
use std::sync::Arc;

use regex::Regex;

use core::codec::Codec;
use core::doc::Document;
use core::index::bufferd_updates::BufferedUpdates;
use core::search::sort::Sort;
use core::store::{DirectoryRc, IOContext};
use core::util::bit_set::FixedBitSet;
use core::util::string_util::ID_LENGTH;
use core::util::{to_base36, Bits, DocId, Version};

use error::{ErrorKind, Result};

// postings flags for postings enum
/// don't need per doc postings
pub const POSTINGS_NONE: i16 = 0;
/// require term frequencies
pub const POSTINGS_FREQS: i16 = 1 << 3;
/// require term positions
pub const POSTINGS_POSITIONS: i16 = POSTINGS_FREQS | 1 << 4;
/// require term offsets
pub const POSTINGS_OFFSETS: i16 = POSTINGS_POSITIONS | 1 << 5;
/// require term payloads
pub const POSTINGS_PAYLOADS: i16 = POSTINGS_POSITIONS | 1 << 6;
/// require positions, payloads and offsets
pub const POSTINGS_ALL: i16 = POSTINGS_OFFSETS | POSTINGS_PAYLOADS;

// index file names
pub const INDEX_FILE_SEGMENTS: &str = "segments";
pub const INDEX_FILE_PENDING_SEGMENTS: &str = "pending_segments";
pub const INDEX_FILE_OLD_SEGMENT_GEN: &str = "segments.gen";

const CODEC_FILE_PATTERN: &str = r"_[a-z0-9]+(_.*)?\..*";

pub fn matches_extension(filename: &str, ext: &str) -> bool {
    filename.ends_with(ext)
}

// locates the boundary of the segment name, or None
fn index_of_segment_name(filename: &str) -> Option<usize> {
    // If it is a .del file, there's an '_' after the first character
    let filename = &filename[1..];
    if let Some(i) = filename.find('_') {
        return Some(i + 1);
    }
    filename.find('.').map(|i| i + 1)
}

pub fn strip_segment_name(name: &str) -> &str {
    if let Some(idx) = index_of_segment_name(name) {
        &name[idx..]
    } else {
        name
    }
}

pub fn segment_file_name(name: &str, suffix: &str, ext: &str) -> String {
    if !ext.is_empty() || !suffix.is_empty() {
        assert!(!ext.starts_with('.'));
        let mut filename = String::with_capacity(name.len() + 2 + suffix.len() + ext.len());
        filename.push_str(name);
        if !suffix.is_empty() {
            filename.push('_');
            filename.push_str(suffix);
        }
        if !ext.is_empty() {
            filename.push('.');
            filename.push_str(ext);
        }
        filename
    } else {
        String::from(name)
    }
}

pub fn file_name_from_generation(base: &str, ext: &str, gen: u64) -> String {
    if gen == 0 {
        segment_file_name(base, "", ext)
    } else {
        let mut res = String::new();
        res.push_str(base);
        res.push('_');
        res += &to_base36(gen);
        if !ext.is_empty() {
            res.push('.');
            res.push_str(ext);
        }
        res
    }
}

/// Returns the generation from this file name,
/// or 0 if there is no generation
pub fn parse_generation(filename: &str) -> Result<i64> {
    debug_assert!(filename.starts_with("_"));
    let parts: Vec<&str> = strip_extension(filename)[1..].split("_").collect();
    // 4 cases:
    // segment.ext
    // segment_gen.ext
    // segment_codec_suffix.ext
    // segment_gen_codec_suffix.ext
    if parts.len() == 2 || parts.len() == 4 {
        Ok(parts[1].parse()?)
    } else {
        Ok(0)
    }
}

/// Parses the segment name out of the given file name.
/// @return the segment name only, or filename if it
/// does not contain a '.' and '_'.
pub fn parse_segment_name(filename: &str) -> &str {
    if let Some(idx) = index_of_segment_name(filename) {
        &filename[..idx]
    } else {
        filename
    }
}

/// Removes the extension (anything after the first '.'),
/// otherwise returns the original filename.
fn strip_extension(filename: &str) -> &str {
    if let Some(idx) = filename.find('.') {
        &filename[..idx]
    } else {
        filename
    }
}

pub trait IndexReader: Send + Sync {
    fn leaves(&self) -> Vec<&LeafReader>;
    fn term_vector(&self, doc_id: DocId) -> Result<Option<Box<Fields>>>;
    fn document(&self, doc_id: DocId, fields: &[String]) -> Result<Document>;
    fn max_doc(&self) -> i32;
    fn num_docs(&self) -> i32;
    fn num_deleted_docs(&self) -> i32 {
        self.max_doc() - self.num_docs()
    }
    fn has_deletions(&self) -> bool {
        self.num_deleted_docs() > 0
    }
    fn leaf_reader_for_doc(&self, doc: DocId) -> &LeafReader {
        let leaves = self.leaves();
        let size = leaves.len();
        let mut lo = 0usize;
        let mut hi = size - 1;
        while hi >= lo {
            let mut mid = (lo + hi) >> 1;
            let mid_value = leaves[mid].doc_base();
            if doc < mid_value {
                hi = mid - 1;
            } else if doc > mid_value {
                lo = mid + 1;
            } else {
                while mid + 1 < size && leaves[mid + 1].doc_base() == mid_value {
                    mid += 1;
                }
                return leaves[mid];
            }
        }
        leaves[hi]
    }

    fn as_any(&self) -> &Any;
}

pub type IndexReaderRef = Arc<IndexReader>;

pub const SEGMENT_INFO_YES: i32 = 1;
pub const SEGMENT_INFO_NO: i32 = -1;

pub struct SegmentInfo {
    pub name: String,
    pub max_doc: i32,
    pub directory: DirectoryRc,
    pub is_compound_file: AtomicBool,
    pub id: [u8; ID_LENGTH],
    pub codec: Option<Arc<Codec>>,
    pub diagnostics: HashMap<String, String>,
    pub attributes: HashMap<String, String>,
    pub index_sort: Option<Sort>,
    pub version: Version,
    pub set_files: HashSet<String>,
}

impl SegmentInfo {
    #[allow(too_many_arguments)]
    pub fn new(
        version: Version,
        name: &str,
        max_doc: i32,
        directory: DirectoryRc,
        is_compound_file: bool,
        codec: Option<Arc<Codec>>,
        diagnostics: HashMap<String, String>,
        id: [u8; ID_LENGTH],
        attributes: HashMap<String, String>,
        index_sort: Option<Sort>,
    ) -> Result<SegmentInfo> {
        Ok(SegmentInfo {
            name: String::from(name),
            max_doc,
            directory,
            is_compound_file: AtomicBool::new(is_compound_file),
            id,
            version,
            codec,
            diagnostics,
            attributes,
            set_files: HashSet::new(),
            index_sort,
        })
    }

    pub fn set_codec(&mut self, codec: Arc<Codec>) {
        self.codec = Some(codec);
    }

    pub fn codec(&self) -> &Arc<Codec> {
        assert!(self.codec.is_some());
        &self.codec.as_ref().unwrap()
    }

    pub fn max_doc(&self) -> i32 {
        self.max_doc
    }

    pub fn is_compound_file(&self) -> bool {
        self.is_compound_file.load(AtomicOrdering::Acquire)
    }

    pub fn set_use_compound_file(&self) {
        self.is_compound_file.store(true, AtomicOrdering::Release)
    }

    pub fn get_id(&self) -> &[u8] {
        &self.id
    }

    /// Return all files referenced by this SegmentInfo.
    pub fn files(&self) -> &HashSet<String> {
        // debug_assert!(!self.set_files.is_empty());
        &self.set_files
    }

    pub fn set_files(&mut self, files: &HashSet<String>) -> Result<()> {
        self.set_files = HashSet::with_capacity(files.len());
        self.add_files(files)
    }

    pub fn add_file(&mut self, file: &str) -> Result<()> {
        self.check_file_name(file)?;
        let file = self.named_for_this_segment(file);
        self.set_files.insert(file);
        Ok(())
    }

    pub fn add_files(&mut self, files: &HashSet<String>) -> Result<()> {
        for f in files {
            self.check_file_name(f)?;
        }
        for f in files {
            let file = self.named_for_this_segment(&f);
            self.set_files.insert(file);
        }
        Ok(())
    }

    fn check_file_name(&self, file: &str) -> Result<()> {
        let pattern = Regex::new(CODEC_FILE_PATTERN).unwrap();
        if !pattern.is_match(file) {
            bail!(ErrorKind::IllegalArgument("invalid code file_name.".into()));
        }
        if file.to_lowercase().ends_with(".tmp") {
            bail!(ErrorKind::IllegalArgument(
                "invalid code file_name, can't end with .tmp extension".into()
            ));
        }
        Ok(())
    }

    fn named_for_this_segment(&self, file: &str) -> String {
        let mut name = self.name.clone();
        name.push_str(strip_segment_name(file));
        name
    }

    pub fn index_sort(&self) -> Option<&Sort> {
        self.index_sort.as_ref()
    }

    pub fn set_diagnostics(&mut self, diags: HashMap<String, String>) {
        self.diagnostics = diags;
    }

    pub fn set_max_doc(&mut self, max_doc: i32) -> Result<()> {
        if self.max_doc != -1 {
            bail!(ErrorKind::IllegalState("max_doc was already set".into()));
        }
        self.max_doc = max_doc;
        Ok(())
    }
}

impl Clone for SegmentInfo {
    fn clone(&self) -> Self {
        SegmentInfo {
            name: self.name.clone(),
            max_doc: self.max_doc,
            is_compound_file: AtomicBool::new(self.is_compound_file()),
            directory: Arc::clone(&self.directory),
            id: self.id.clone(),
            codec: self.codec.as_ref().map(|c| Arc::clone(c)),
            diagnostics: self.diagnostics.clone(),
            attributes: self.attributes.clone(),
            version: self.version.clone(),
            set_files: self.set_files.clone(),
            index_sort: self.index_sort.clone(),
        }
    }
}

impl Hash for SegmentInfo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(self.name.as_bytes());
    }
}

impl Serialize for SegmentInfo {
    fn serialize<S>(&self, serializer: S) -> result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("SegmentInfo", 8)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("max_doc", &self.max_doc)?;
        s.serialize_field("is_compound_file", &self.is_compound_file())?;
        s.serialize_field("id", &self.id)?;
        // TODO: directory?
        if self.codec.is_some() {
            let codec = self.codec.as_ref().unwrap();
            s.serialize_field("codec", codec.name())?;
        } else {
            s.serialize_field("codec", "uninitialized")?;
        };
        s.serialize_field("diagnostics", &self.diagnostics)?;
        s.serialize_field("attributes", &self.attributes)?;
        s.serialize_field("version", &self.version)?;
        s.end()
    }
}

impl fmt::Debug for SegmentInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Ok(s) = ::serde_json::to_string_pretty(self) {
            write!(f, "{}", s)?;
        }

        Ok(())
    }
}

/// A Term represents a word from text.  This is the unit of search.  It is
/// composed of two elements, the text of the word, as a string, and the name of
/// the field that the text occurred in.
///
/// Note that terms may represent more than words from text fields, but also
/// things like dates, email addresses, urls, etc.
#[derive(Clone, Debug, PartialEq, Hash, Eq)]
pub struct Term {
    pub field: String,
    pub bytes: Vec<u8>,
}

impl Term {
    /// Constructs a Term with the given field and bytes.
    /// <p>Note that a null field or null bytes value results in undefined
    /// behavior for most Lucene APIs that accept a Term parameter.
    ///
    /// <p>The provided BytesRef is copied when it is non null.
    pub fn new(field: String, bytes: Vec<u8>) -> Term {
        Term { field, bytes }
    }

    /// Returns the field of this term.   The field indicates
    /// the part of a document which this term came from.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Returns the text of this term.  In the case of words, this is simply the
    /// text of the word.  In the case of dates and other types, this is an
    /// encoding of the object as a string.
    pub fn text(&self) -> Result<String> {
        Ok(String::from_utf8(self.bytes.clone())?)
    }

    pub fn is_empty(&self) -> bool {
        self.field.is_empty() && self.bytes.is_empty()
    }

    pub fn copy_bytes(&mut self, bytes: &[u8]) {
        if self.bytes.len() != bytes.len() {
            self.bytes.resize(bytes.len(), 0);
        }
        self.bytes.copy_from_slice(bytes);
    }
}

impl PartialOrd for Term {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Term {
    fn cmp(&self, other: &Self) -> Ordering {
        let res = self.field.cmp(&other.field);
        if res == Ordering::Equal {
            self.bytes.cmp(&other.bytes)
        } else {
            res
        }
    }
}

pub struct TermContext {
    pub doc_freq: i32,
    pub total_term_freq: i64,
    pub states: Vec<(DocId, Box<TermState>)>,
}

impl TermContext {
    pub fn new(reader: &IndexReader) -> TermContext {
        let doc_freq = 0;
        let total_term_freq = 0;
        let states = Vec::with_capacity(reader.leaves().len());
        TermContext {
            doc_freq,
            total_term_freq,
            states,
        }
    }

    pub fn build(&mut self, reader: &IndexReader, term: &Term) -> Result<()> {
        for reader in reader.leaves() {
            if let Some(terms) = reader.terms(&term.field)? {
                let mut terms_enum = terms.iterator()?;
                if terms_enum.seek_exact(&term.bytes)? {
                    // TODO add TermStates if someone need it
                    let doc_freq = terms_enum.doc_freq()?;
                    let total_term_freq = terms_enum.total_term_freq()?;
                    self.accumulate_statistics(doc_freq, total_term_freq as i64);
                    self.states
                        .push((reader.doc_base(), terms_enum.term_state()?));
                }
            }
        }

        Ok(())
    }

    fn accumulate_statistics(&mut self, doc_freq: i32, total_term_freq: i64) {
        self.doc_freq += doc_freq;
        if self.total_term_freq >= 0 && total_term_freq >= 0 {
            self.total_term_freq += total_term_freq
        } else {
            self.total_term_freq = -1
        }
    }

    pub fn get_term_state(&self, reader: &LeafReader) -> Option<&TermState> {
        for (doc_base, state) in &self.states {
            if *doc_base == reader.doc_base() {
                return Some(state.as_ref());
            }
        }
        None
    }

    pub fn term_states(&self) -> HashMap<DocId, Box<TermState>> {
        let mut term_states: HashMap<DocId, Box<TermState>> = HashMap::new();
        for (doc_base, term_state) in &self.states {
            term_states.insert(*doc_base, term_state.clone_to());
        }

        term_states
    }
}

/// Embeds a [read-only] SegmentInfo and adds per-commit
/// fields.
/// @lucene.experimental */
pub struct SegmentCommitInfo {
    /// The {@link SegmentInfo} that we wrap.
    pub info: SegmentInfo,
    /// How many deleted docs in the segment:
    pub del_count: AtomicI32,
    /// Generation number of the live docs file (-1 if there
    /// are no deletes yet):
    pub del_gen: AtomicI64,
    /// Normally 1+delGen, unless an exception was hit on last
    /// attempt to write:
    pub next_write_del_gen: AtomicI64,
    /// Generation number of the FieldInfos (-1 if there are no updates)
    field_infos_gen: AtomicI64,
    /// Normally 1+fieldInfosGen, unless an exception was hit on last attempt to
    /// write
    pub next_write_field_infos_gen: AtomicI64,
    /// Generation number of the DocValues (-1 if there are no updates)
    pub doc_values_gen: i64,
    /// Normally 1+dvGen, unless an exception was hit on last attempt to
    /// write
    pub next_write_doc_values_gen: AtomicI64,
    /// Track the per-field DocValues update files
    pub dv_updates_files: HashMap<i32, HashSet<String>>,
    /// TODO should we add .files() to FieldInfosFormat, like we have on
    /// LiveDocsFormat?
    /// track the fieldInfos update files
    pub field_infos_files: HashSet<String>,

    pub size_in_bytes: AtomicI64,
    // NOTE: only used in-RAM by IW to track buffered deletes;
    // this is never written to/read from the Directory
    pub buffered_deletes_gen: AtomicI64,
}

impl Hash for SegmentCommitInfo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.info.hash(state);
    }
}

impl SegmentCommitInfo {
    pub fn new(
        info: SegmentInfo,
        del_count: i32,
        del_gen: i64,
        field_infos_gen: i64,
        doc_values_gen: i64,
        dv_updates_files: HashMap<i32, HashSet<String>>,
        field_infos_files: HashSet<String>,
    ) -> SegmentCommitInfo {
        let field_info_gen = if field_infos_gen == -1 {
            1
        } else {
            field_infos_gen + 1
        };
        SegmentCommitInfo {
            info,
            del_count: AtomicI32::new(del_count),
            del_gen: AtomicI64::new(del_gen),
            next_write_del_gen: AtomicI64::new(if del_gen == -1 { 1i64 } else { del_gen + 1 }),
            field_infos_gen: AtomicI64::new(field_infos_gen),
            next_write_field_infos_gen: AtomicI64::new(field_info_gen),
            doc_values_gen,
            next_write_doc_values_gen: AtomicI64::new(if doc_values_gen == -1 {
                1
            } else {
                doc_values_gen + 1
            }),
            dv_updates_files,
            field_infos_files,
            size_in_bytes: AtomicI64::new(-1),
            buffered_deletes_gen: AtomicI64::new(0),
        }
    }

    pub fn files(&self) -> HashSet<String> {
        let mut files = HashSet::new();
        // Start from the wrapped info's files:
        for f in self.info.files() {
            files.insert(f.clone());
        }
        // TODO we could rely on TrackingDir.getCreatedFiles() (like we do for
        // updates) and then maybe even be able to remove LiveDocsFormat.files().

        // Must separately add any live docs files:
        self.info.codec().live_docs_format().files(self, &mut files);

        // must separately add any field updates files
        for (_, fs) in &self.dv_updates_files {
            for f in fs {
                files.insert(f.clone());
            }
        }

        // must separately add field_infos files
        for f in &self.field_infos_files {
            files.insert(f.clone());
        }

        files
    }

    pub fn has_deletions(&self) -> bool {
        self.del_gen() != -1
    }

    pub fn del_count(&self) -> i32 {
        self.del_count.load(AtomicOrdering::Acquire)
    }

    pub fn set_del_count(&self, del_count: i32) -> Result<()> {
        if del_count < 0 || del_count > self.info.max_doc() {
            bail!(ErrorKind::IllegalArgument("invalid del_count".into()));
        }
        self.del_count.store(del_count, AtomicOrdering::Release);
        Ok(())
    }

    pub fn has_field_updates(&self) -> bool {
        self.field_infos_gen() != -1
    }

    pub fn field_infos_gen(&self) -> i64 {
        self.field_infos_gen.load(AtomicOrdering::Acquire)
    }

    pub fn next_write_field_infos_gen(&self) -> i64 {
        self.next_write_field_infos_gen
            .load(AtomicOrdering::Acquire)
    }

    pub fn set_next_write_field_infos_gen(&self, gen: i64) {
        self.next_write_field_infos_gen
            .store(gen, AtomicOrdering::Release)
    }

    pub fn next_write_doc_values_gen(&self) -> i64 {
        self.next_write_doc_values_gen.load(AtomicOrdering::Acquire)
    }

    pub fn set_next_write_doc_values_gen(&self, gen: i64) {
        self.next_write_doc_values_gen
            .store(gen, AtomicOrdering::Release);
    }

    pub fn advance_field_infos_gen(&self) {
        self.field_infos_gen
            .store(self.next_field_infos_gen(), AtomicOrdering::Release);
        self.next_write_field_infos_gen
            .store(self.field_infos_gen() + 1, AtomicOrdering::Release);
        self.size_in_bytes.store(-1, AtomicOrdering::Release);
    }

    pub fn next_write_del_gen(&self) -> i64 {
        self.next_write_del_gen.load(AtomicOrdering::Acquire)
    }

    pub fn set_next_write_del_gen(&self, gen: i64) {
        self.next_write_del_gen.store(gen, AtomicOrdering::Release)
    }

    pub fn next_field_infos_gen(&self) -> i64 {
        self.next_write_field_infos_gen
            .load(AtomicOrdering::Acquire)
    }

    pub fn advance_next_write_del_gen(&self) {
        self.next_write_del_gen
            .fetch_add(1, AtomicOrdering::Acquire);
    }

    pub fn del_gen(&self) -> i64 {
        self.del_gen.load(AtomicOrdering::Acquire)
    }

    pub fn advance_del_gen(&self) {
        self.del_gen.store(
            self.next_write_del_gen.load(AtomicOrdering::Acquire),
            AtomicOrdering::Release,
        );
        self.next_write_del_gen
            .store(self.del_gen() + 1, AtomicOrdering::Release);
        self.size_in_bytes.store(-1, AtomicOrdering::Release);
    }

    pub fn size_in_bytes(&self) -> i64 {
        let mut size = self.size_in_bytes.load(AtomicOrdering::Acquire);
        if size == -1 {
            let mut sum = 0;
            for name in self.files() {
                match self.info.directory.file_length(&name) {
                    Ok(l) => {
                        sum += l;
                    }
                    Err(e) => {
                        warn!("get file '{}' length failed by '{:?}'", name, e);
                    }
                }
            }
            size = sum;
            self.size_in_bytes.store(size, AtomicOrdering::Release);
        }
        size
    }

    pub fn buffered_deletes_gen(&self) -> i64 {
        self.buffered_deletes_gen.load(AtomicOrdering::Acquire)
    }

    pub fn set_buffered_deletes_gen(&self, v: i64) {
        self.buffered_deletes_gen.store(v, AtomicOrdering::Release);
        self.size_in_bytes.store(-1, AtomicOrdering::Release);
    }
}

impl Clone for SegmentCommitInfo {
    fn clone(&self) -> Self {
        let infos = SegmentCommitInfo::new(
            self.info.clone(),
            self.del_count(),
            self.del_gen(),
            self.field_infos_gen(),
            self.doc_values_gen,
            self.dv_updates_files.clone(),
            self.field_infos_files.clone(),
        );
        // Not clear that we need to carry over nextWriteDelGen
        // (i.e. do we ever clone after a failed write and
        // before the next successful write?), but just do it to
        // be safe:
        infos
            .next_write_del_gen
            .store(self.next_write_del_gen(), AtomicOrdering::Release);
        infos
            .next_write_field_infos_gen
            .store(self.next_write_field_infos_gen(), AtomicOrdering::Release);
        infos.set_next_write_doc_values_gen(self.next_write_doc_values_gen());
        infos
    }
}

impl Eq for SegmentCommitInfo {}

// TODO, only compare the segment name, maybe we should compare the raw pointer or the full struct?
impl PartialEq for SegmentCommitInfo {
    fn eq(&self, other: &SegmentCommitInfo) -> bool {
        self.info.name.eq(&other.info.name)
    }
}

impl Serialize for SegmentCommitInfo {
    fn serialize<S>(&self, serializer: S) -> result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("SegmentCommitInfo", 11)?;
        s.serialize_field("info", &self.info)?;
        s.serialize_field("del_count", &self.del_count())?;
        s.serialize_field("del_gen", &self.del_gen())?;
        s.serialize_field("next_write_del_gen", &self.next_write_del_gen())?;
        s.serialize_field("field_infos_gen", &self.field_infos_gen())?;
        s.serialize_field(
            "next_write_field_infos_gen",
            &self
                .next_write_field_infos_gen
                .load(AtomicOrdering::Acquire),
        )?;
        s.serialize_field("doc_values_gen", &self.doc_values_gen)?;
        s.serialize_field(
            "next_write_doc_values_gen",
            &self.next_write_doc_values_gen(),
        )?;
        s.serialize_field("dv_updates_files", &self.dv_updates_files)?;
        s.serialize_field("field_infos_files", &self.field_infos_files)?;
        s.serialize_field("size_in_bytes", &self.size_in_bytes())?;
        s.end()
    }
}

impl fmt::Display for SegmentCommitInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Ok(s) = ::serde_json::to_string_pretty(self) {
            write!(f, "{}", s)?;
        }

        Ok(())
    }
}

impl fmt::Debug for SegmentCommitInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Ok(s) = ::serde_json::to_string_pretty(self) {
            write!(f, "{}", s)?;
        }

        Ok(())
    }
}

/// Holder class for common parameters used during write.
/// @lucene.experimental
pub struct SegmentWriteState {
    /// {@link InfoStream} used for debugging messages. */
    // info_stream: InfoStream,
    /// {@link Directory} where this segment will be written
    /// to.
    pub directory: DirectoryRc,

    /// {@link SegmentInfo} describing this segment. */
    pub segment_info: SegmentInfo,

    /// {@link FieldInfos} describing all fields in this
    /// segment. */
    pub field_infos: FieldInfos,

    /// Number of deleted documents set while flushing the
    /// segment. */
    pub del_count_on_flush: u32,

    /// Deletes and updates to apply while we are flushing the segment. A Term is
    /// enrolled in here if it was deleted/updated at one point, and it's mapped to
    /// the docIDUpto, meaning any docID &lt; docIDUpto containing this term should
    /// be deleted/updated.
    pub seg_updates: Option<*const BufferedUpdates>,

    /// {@link MutableBits} recording live documents; this is
    /// only set if there is one or more deleted documents. */
    live_docs: Box<Bits>,

    /// Unique suffix for any postings files written for this
    /// segment.  {@link PerFieldPostingsFormat} sets this for
    /// each of the postings formats it wraps.  If you create
    /// a new {@link PostingsFormat} then any files you
    /// write/read must be derived using this suffix (use
    /// {@link IndexFileNames#segmentFileName(String,String,String)}).
    ///
    /// Note: the suffix must be either empty, or be a textual suffix contain exactly two parts
    /// (separated by underscore), or be a base36 generation. */
    pub segment_suffix: String,

    /// {@link IOContext} for all writes; you should pass this
    /// to {@link Directory#createOutput(String,IOContext)}. */
    pub context: IOContext,
}

impl SegmentWriteState {
    pub fn new(
        directory: DirectoryRc,
        segment_info: SegmentInfo,
        field_infos: FieldInfos,
        seg_updates: Option<*const BufferedUpdates>,
        context: IOContext,
        segment_suffix: String,
    ) -> Self {
        debug_assert!(Self::assert_segment_suffix(&segment_suffix));
        SegmentWriteState {
            directory,
            segment_info,
            field_infos,
            del_count_on_flush: 0,
            seg_updates,
            live_docs: Box::new(FixedBitSet::default()),
            segment_suffix,
            context,
        }
    }

    pub fn seg_updates(&self) -> &BufferedUpdates {
        unsafe { &*self.seg_updates.unwrap() }
    }

    // currently only used by assert? clean up and make real check?
    // either it's a segment suffix (_X_Y) or it's a parseable generation
    // TODO: this is very confusing how ReadersAndUpdates passes generations via
    // this mechanism, maybe add 'generation' explicitly to ctor create the 'actual suffix' here?
    fn assert_segment_suffix(segment_suffix: &str) -> bool {
        if !segment_suffix.is_empty() {
            let parts: Vec<&str> = segment_suffix.split("_").collect();
            if parts.len() == 2 {
                true
            } else if parts.len() == 1 {
                segment_suffix.parse::<i64>().is_ok()
            } else {
                false // invalid
            }
        } else {
            true
        }
    }
}

impl Clone for SegmentWriteState {
    fn clone(&self) -> Self {
        SegmentWriteState {
            directory: Arc::clone(&self.directory),
            segment_info: self.segment_info.clone(),
            field_infos: self.field_infos.clone(),
            del_count_on_flush: self.del_count_on_flush,
            seg_updates: None,
            // no used
            live_docs: Box::new(FixedBitSet::default()),
            // TODO, fake clone
            segment_suffix: self.segment_suffix.clone(),
            context: self.context,
        }
    }
}

/// Holder class for common parameters used during read.
/// @lucene.experimental
pub struct SegmentReadState<'a> {
    /// {@link Directory} where this segment is read from.
    pub directory: DirectoryRc,

    /// {@link SegmentInfo} describing this segment.
    pub segment_info: &'a SegmentInfo,

    /// {@link FieldInfos} describing all fields in this
    /// segment. */
    pub field_infos: Arc<FieldInfos>,

    /// {@link IOContext} to pass to {@link
    /// Directory#openInput(String,IOContext)}.
    pub context: &'a IOContext,

    /// Unique suffix for any postings files read for this
    /// segment.  {@link PerFieldPostingsFormat} sets this for
    /// each of the postings formats it wraps.  If you create
    /// a new {@link PostingsFormat} then any files you
    /// write/read must be derived using this suffix (use
    /// {@link IndexFileNames#segmentFileName(String,String,String)}).
    pub segment_suffix: String,
}

impl<'a> SegmentReadState<'a> {
    pub fn new(
        directory: DirectoryRc,
        segment_info: &'a SegmentInfo,
        field_infos: Arc<FieldInfos>,
        context: &'a IOContext,
        segment_suffix: String,
    ) -> SegmentReadState<'a> {
        SegmentReadState {
            directory,
            segment_info,
            field_infos,
            context,
            segment_suffix,
        }
    }

    pub fn with_suffix(state: &'a SegmentReadState, suffix: &str) -> SegmentReadState<'a> {
        Self::new(
            state.directory.clone(),
            state.segment_info,
            state.field_infos.clone(),
            state.context,
            String::from(suffix),
        )
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::HashMap;

    use super::*;
    use core::codec::{DocValuesProducer, FieldsProducer};
    use core::codec::{NormsProducer, StoredFieldsReader, TermVectorsReader};
    use core::index::point_values::PointValuesRef;
    use core::search::bm25_similarity::BM25Similarity;
    use core::util::external::deferred::Deferred;
    use core::util::*;

    use core::codec::FieldsProducerRef;

    pub struct MockNumericValues {
        num: HashMap<i32, u8>,
    }

    impl Default for MockNumericValues {
        fn default() -> MockNumericValues {
            let mut num = HashMap::<i32, u8>::new();

            let norm_value = BM25Similarity::encode_norm_value(1f32, 120);
            num.insert(1, norm_value);
            let norm_value = BM25Similarity::encode_norm_value(1f32, 1000);
            num.insert(2, norm_value);
            MockNumericValues { num }
        }
    }

    impl NumericDocValues for MockNumericValues {
        fn get_with_ctx(
            &self,
            ctx: NumericDocValuesContext,
            doc_id: DocId,
        ) -> Result<(i64, NumericDocValuesContext)> {
            Ok((i64::from(self.num[&doc_id]), ctx))
        }
    }

    #[derive(Default)]
    pub struct MockBits {}

    impl Bits for MockBits {
        fn get_with_ctx(&self, ctx: BitsContext, _index: usize) -> Result<(bool, BitsContext)> {
            Ok((true, ctx))
        }

        fn len(&self) -> usize {
            unimplemented!()
        }
    }

    pub struct MockLeafReader {
        doc_base: DocId,
        live_docs: BitsRef,
        field_infos: FieldInfos,
    }

    impl MockLeafReader {
        pub fn new(doc_base: DocId) -> MockLeafReader {
            let mut infos = Vec::new();
            let field_info_one = FieldInfo::new(
                "test".to_string(),
                1,
                true,
                true,
                false,
                IndexOptions::Docs,
                DocValuesType::Numeric,
                1,
                HashMap::new(),
                1,
                1,
            )
            .unwrap();
            let field_info_two = FieldInfo::new(
                "test_2".to_string(),
                2,
                true,
                true,
                false,
                IndexOptions::Docs,
                DocValuesType::SortedNumeric,
                2,
                HashMap::new(),
                2,
                2,
            )
            .unwrap();
            infos.push(field_info_one);
            infos.push(field_info_two);

            MockLeafReader {
                doc_base,
                live_docs: Arc::new(MatchAllBits::new(0usize)),
                field_infos: FieldInfos::new(infos).unwrap(),
            }
        }
    }

    impl LeafReader for MockLeafReader {
        fn add_core_drop_listener(&self, listener: Deferred) {
            unreachable!()
        }

        fn doc_base(&self) -> DocId {
            self.doc_base
        }

        fn name(&self) -> &str {
            "test"
        }

        fn fields(&self) -> Result<FieldsProducerRef> {
            unimplemented!()
        }

        fn term_vector(&self, _doc_id: DocId) -> Result<Option<Box<Fields>>> {
            unimplemented!()
        }

        fn point_values(&self) -> Option<PointValuesRef> {
            unimplemented!()
        }

        fn document(&self, _doc_id: DocId, _visitor: &mut StoredFieldVisitor) -> Result<()> {
            unimplemented!()
        }

        fn norm_values(&self, _field: &str) -> Result<Option<Box<NumericDocValues>>> {
            Ok(Some(Box::new(MockNumericValues::default())))
        }

        fn live_docs(&self) -> BitsRef {
            Arc::clone(&self.live_docs)
        }

        fn field_infos(&self) -> &FieldInfos {
            &self.field_infos
        }

        fn clone_field_infos(&self) -> Arc<FieldInfos> {
            unimplemented!()
        }

        fn max_doc(&self) -> DocId {
            0
        }

        fn get_docs_with_field(&self, _field: &str) -> Result<BitsRef> {
            Ok(Arc::new(MockBits::default()))
        }

        fn get_numeric_doc_values(&self, _field: &str) -> Result<NumericDocValuesRef> {
            Ok(Arc::new(MockNumericValues::default()))
        }

        fn get_binary_doc_values(&self, _field: &str) -> Result<BinaryDocValuesRef> {
            unimplemented!()
        }

        fn get_sorted_doc_values(&self, _field: &str) -> Result<SortedDocValuesRef> {
            unimplemented!()
        }

        fn get_sorted_numeric_doc_values(&self, _field: &str) -> Result<SortedNumericDocValuesRef> {
            // TODO fix this
            // let boxed = Box::new(MockSortedNumericDocValues::new());
            // Ok(Arc::new(Mutex::new(boxed)))
            //
            unimplemented!()
        }

        fn get_sorted_set_doc_values(&self, _field: &str) -> Result<SortedSetDocValuesRef> {
            unimplemented!()
        }

        fn field_info(&self, _field: &str) -> Option<&FieldInfo> {
            unimplemented!()
        }

        fn num_docs(&self) -> i32 {
            0
        }

        fn core_cache_key(&self) -> &str {
            unimplemented!()
        }
        fn is_codec_reader(&self) -> bool {
            false
        }

        fn index_sort(&self) -> Option<&Sort> {
            None
        }

        fn store_fields_reader(&self) -> Result<Arc<StoredFieldsReader>> {
            unreachable!()
        }

        fn term_vectors_reader(&self) -> Result<Option<Arc<TermVectorsReader>>> {
            unreachable!()
        }

        fn norms_reader(&self) -> Result<Option<Arc<NormsProducer>>> {
            unreachable!()
        }

        fn doc_values_reader(&self) -> Result<Option<Arc<DocValuesProducer>>> {
            unreachable!()
        }

        fn postings_reader(&self) -> Result<Arc<FieldsProducer>> {
            unreachable!()
        }
    }

    pub struct MockIndexReader {
        leaves: Vec<MockLeafReader>,
    }

    impl MockIndexReader {
        pub fn new(leaves: Vec<MockLeafReader>) -> MockIndexReader {
            MockIndexReader { leaves }
        }
    }

    impl IndexReader for MockIndexReader {
        fn leaves(&self) -> Vec<&LeafReader> {
            let mut leaves: Vec<&LeafReader> = vec![];
            for leaf in &self.leaves {
                leaves.push(leaf);
            }
            leaves
        }

        fn term_vector(&self, _doc_id: DocId) -> Result<Option<Box<Fields>>> {
            unimplemented!()
        }

        fn document(&self, _doc_id: DocId, _fields_load: &[String]) -> Result<Document> {
            unimplemented!()
        }

        fn max_doc(&self) -> i32 {
            1
        }

        fn num_docs(&self) -> i32 {
            1
        }

        fn as_any(&self) -> &Any {
            self
        }
    }
}
