use error::{ErrorKind, Result};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use core::codec::codec_util;
use core::codec::format::CompoundFormat;
use core::store::{Directory, DirectoryRc, Lock};
use core::store::{IOContext, IO_CONTEXT_READONCE};
use core::store::{IndexInput, IndexOutput};

use core::index::{segment_file_name, strip_segment_name, SegmentInfo};

const DATA_EXTENSION: &str = "cfs";
/// Extension of compound file entries */
pub const ENTRIES_EXTENSION: &str = "cfe";
pub const DATA_CODEC: &str = "Lucene50CompoundData";
pub const ENTRY_CODEC: &str = "Lucene50CompoundEntries";
pub const VERSION_START: i32 = 0;
pub const VERSION_CURRENT: i32 = VERSION_START;

pub struct Lucene50CompoundFormat {}

impl Lucene50CompoundFormat {}

impl CompoundFormat for Lucene50CompoundFormat {
    fn get_compound_reader(
        &self,
        dir: DirectoryRc,
        si: &SegmentInfo,
        ioctx: &IOContext,
    ) -> Result<DirectoryRc> {
        let reader = Lucene50CompoundReader::new(dir, si, ioctx)?;
        Ok(Arc::new(reader))
    }

    fn write(&self, dir: &Directory, si: &SegmentInfo, ctx: &IOContext) -> Result<()> {
        let data_file = segment_file_name(&si.name, "", DATA_EXTENSION);
        let entries_file = segment_file_name(&si.name, "", ENTRIES_EXTENSION);

        let mut data = dir.create_output(&data_file, ctx)?;
        let mut entries = dir.create_output(&entries_file, ctx)?;

        codec_util::write_index_header(
            data.as_mut(),
            DATA_CODEC,
            VERSION_CURRENT,
            si.get_id(),
            "",
        )?;
        codec_util::write_index_header(
            entries.as_mut(),
            ENTRY_CODEC,
            VERSION_CURRENT,
            si.get_id(),
            "",
        )?;

        // write number of files
        entries.write_vint(si.files().len() as i32)?;
        for file in si.files() {
            // write bytes for file
            let start_offset = data.file_pointer();

            let mut input = dir.open_checksum_input(file, &IOContext::Read(true))?;

            // just copies the index header, verifying that its id matches what we expect
            codec_util::verify_and_copy_index_header(input.as_mut(), data.as_mut(), si.get_id())?;

            // copy all bytes except the footer
            let num_bytes_to_copy =
                input.len() as usize - codec_util::footer_length() - input.file_pointer() as usize;
            data.copy_bytes(input.as_data_input(), num_bytes_to_copy)?;

            // verify footer (checksum) matches for the incoming file we are copying
            let checksum = codec_util::check_footer(input.as_mut())?;

            // this is poached from codec_util::write_footer, be we need to use our own checksum
            // not data.checksum(), but I think adding a public method to codec_util to do that
            // is somewhat dangerous:
            data.write_int(codec_util::FOOTER_MAGIC)?;
            data.write_int(0)?;
            data.write_long(checksum)?;

            let end_offset = data.file_pointer();
            let length = end_offset - start_offset;

            // write entry for file
            entries.write_string(strip_segment_name(file))?;
            entries.write_long(start_offset)?;
            entries.write_long(length)?;
        }

        codec_util::write_footer(data.as_mut())?;
        codec_util::write_footer(entries.as_mut())
    }
}

/// Offset/Length for a slice inside of a compound file */
#[derive(Debug)]
pub struct FileEntry(i64, i64);

/// Class for accessing a compound stream.
/// This class implements a directory, but is limited to only read operations.
/// Directory methods that would normally modify data throw an exception.
pub struct Lucene50CompoundReader {
    pub directory: DirectoryRc,
    name: String,
    entries: HashMap<String, FileEntry>,
    input: Box<IndexInput>,
    pub version: i32,
}

impl Lucene50CompoundReader {
    /// Create a new CompoundFileDirectory.
    // TODO: we should just pre-strip "entries" and append segment name up-front like simpletext?
    // this need not be a "general purpose" directory anymore (it only writes index files)
    pub fn new(
        directory: DirectoryRc,
        si: &SegmentInfo,
        context: &IOContext,
    ) -> Result<Lucene50CompoundReader> {
        let data_file_name = segment_file_name(si.name.as_ref(), "", DATA_EXTENSION);
        let entries_file_name = segment_file_name(si.name.as_ref(), "", ENTRIES_EXTENSION);
        let (version, entries) =
            Lucene50CompoundReader::read_entries(si.id.as_ref(), &directory, &entries_file_name)?;
        let mut expected_length = codec_util::index_header_length(DATA_CODEC, "") as u64;
        for v in entries.values() {
            expected_length += v.1 as u64; // 1 for length
        }
        expected_length += codec_util::footer_length() as u64;

        let mut input = directory.open_input(&data_file_name, context)?;
        codec_util::check_index_header(
            input.as_mut(),
            DATA_CODEC,
            version,
            version,
            si.id.as_ref(),
            "",
        )?;
        codec_util::retrieve_checksum(input.as_mut())?;
        if input.as_ref().len() != expected_length {
            return Err(format!(
                "length should be {} bytes, but is {} instead",
                expected_length,
                input.as_ref().len()
            )
            .into());
        }
        Ok(Lucene50CompoundReader {
            directory,
            name: si.name.clone(),
            entries,
            input,
            version,
        })
    }

    /// Helper method that reads CFS entries from an input stream
    pub fn read_entries(
        segment_id: &[u8],
        directory: &DirectoryRc,
        entries_file_name: &str,
    ) -> Result<(i32, HashMap<String, FileEntry>)> {
        let mut entries_stream =
            directory.open_checksum_input(entries_file_name, &IO_CONTEXT_READONCE)?;
        let version = codec_util::check_index_header(
            entries_stream.as_mut(),
            ENTRY_CODEC,
            VERSION_START,
            VERSION_CURRENT,
            segment_id,
            "",
        )?;
        let num_entries = entries_stream.read_vint()?;
        let mut mappings = HashMap::with_capacity(num_entries as usize);
        for _ in 0..num_entries {
            let id = entries_stream.read_string()?;
            let offset = entries_stream.read_long()?;
            let length = entries_stream.read_long()?;
            let previous = mappings.insert(id.clone(), FileEntry(offset, length));
            if previous.is_some() {
                return Err(format!("Duplicate cfs entry id={} in CFS", id).into());
            }
        }

        codec_util::check_footer(entries_stream.as_mut())?;
        Ok((version, mappings))
    }
}

impl Directory for Lucene50CompoundReader {
    /// Returns an array of strings, one for each file in the directory.
    fn list_all(&self) -> Result<Vec<String>> {
        Ok(self
            .entries
            .keys()
            .map(|n| format!("{}{}", self.name, n))
            .collect())
    }

    /// Returns the length of a file in the directory.
    /// @throws IOException if the file does not exist */
    fn file_length(&self, name: &str) -> Result<i64> {
        self.entries
            .get(strip_segment_name(name))
            .map(|e| e.1)
            .ok_or_else(|| "File not Found".into())
    }

    fn obtain_lock(&self, _name: &str) -> Result<Box<Lock>> {
        bail!(ErrorKind::UnsupportedOperation(Cow::Borrowed("")))
    }

    fn create_temp_output(
        &self,
        _prefix: &str,
        _suffix: &str,
        _ctx: &IOContext,
    ) -> Result<Box<IndexOutput>> {
        unimplemented!();
    }

    fn delete_file(&self, _name: &str) -> Result<()> {
        unimplemented!();
    }

    fn sync(&self, _name: &HashSet<String>) -> Result<()> {
        bail!(ErrorKind::UnsupportedOperation(Cow::Borrowed("")))
    }

    fn sync_meta_data(&self) -> Result<()> {
        Ok(())
    }

    fn create_output(&self, _name: &str, _ctx: &IOContext) -> Result<Box<IndexOutput>> {
        unimplemented!()
    }

    fn rename(&self, _source: &str, _dest: &str) -> Result<()> {
        unimplemented!()
    }

    fn open_input(&self, name: &str, _context: &IOContext) -> Result<Box<IndexInput>> {
        let id = strip_segment_name(name);
        let entry = self.entries.get(id).ok_or_else(|| {
            let file_name = segment_file_name(&self.name, "", DATA_EXTENSION);
            format!(
                "No sub-file with id {} found in compound file \"{}\" (fileName={} files: {:?})",
                id,
                file_name,
                name,
                self.entries.keys()
            )
        })?;
        self.input.slice(name, entry.0, entry.1)
    }
}

impl Drop for Lucene50CompoundReader {
    fn drop(&mut self) {}
}

impl fmt::Display for Lucene50CompoundReader {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Lucene50CompoundReader({})", self.directory)
    }
}
