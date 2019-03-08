use core::codec::Codec;
use core::index::bufferd_updates::{self, BufferedUpdates, FrozenBufferUpdates};
use core::index::doc_consumer::{DefaultIndexingChain, DocConsumer};
use core::index::doc_writer_delete_queue::{DeleteSlice, DocumentsWriterDeleteQueue};
use core::index::index_writer::IndexWriter;
use core::index::index_writer::INDEX_MAX_DOCS;
use core::index::index_writer_config::IndexWriterConfig;
use core::index::{FieldInfos, FieldInfosBuilder, FieldNumbers, FieldNumbersRef, Fieldable};
use core::index::{SegmentCommitInfo, SegmentInfo, SegmentWriteState, Term};
use core::search::Similarity;
use core::store::{DirectoryRc, FlushInfo, IOContext, TrackingDirectoryWrapper};
use core::util::byte_block_pool::DirectTrackingAllocator;
use core::util::int_block_pool::{IntAllocator, INT_BLOCK_SIZE};
use core::util::string_util::random_id;
use core::util::BitsRef;
use core::util::DocId;
use core::util::{Count, Counter, VERSION_LATEST};

use std::collections::{HashMap, HashSet};
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::SystemTime;

use error::ErrorKind::IllegalArgument;
use error::Result;

pub struct DocState {
    // analyzer: Analyzer,  // TODO, current Analyzer is not implemented
    // pub similarity: Option<Box<Similarity>>,
    pub doc_id: DocId,
    // pub doc: Vec<Box<Fieldable>>,
}

impl DocState {
    pub fn new() -> Self {
        DocState {
            doc_id: 0,
            // similarity: None,
        }
    }
    pub fn clear(&mut self) {
        // self.doc = Vec::with_capacity(0);
    }
}

pub struct DocumentsWriterPerThread {
    // we should use TrackingDirectoryWrapper instead
    pub directory: DirectoryRc,
    pub directory_orig: DirectoryRc,
    pub doc_state: DocState,
    pub consumer: DefaultIndexingChain,
    pub bytes_used: Counter,
    pending_updates: BufferedUpdates,
    pub segment_info: SegmentInfo,
    // current segment we are working on
    aborted: bool,
    // true if we aborted
    pub num_docs_in_ram: u32,
    pub delete_queue: Arc<DocumentsWriterDeleteQueue>,
    // pointer to DocumentsWriter.delete_queue
    delete_slice: DeleteSlice,
    pub byte_block_allocator: DirectTrackingAllocator,
    pub int_block_allocator: Box<IntAllocator>,
    pending_num_docs: Arc<AtomicI64>,
    index_writer_config: Arc<IndexWriterConfig>,
    // enable_test_points: bool,
    index_writer: *mut IndexWriter,
    pub files_to_delete: HashSet<String>,
    inited: bool,
}

impl DocumentsWriterPerThread {
    pub fn new(
        index_writer: &mut IndexWriter,
        segment_name: String,
        directory_orig: DirectoryRc,
        dir: DirectoryRc,
        index_writer_config: Arc<IndexWriterConfig>,
        delete_queue: Arc<DocumentsWriterDeleteQueue>,
        pending_num_docs: Arc<AtomicI64>,
    ) -> Result<Self> {
        let directory = Arc::new(TrackingDirectoryWrapper::new(dir));
        let segment_info = SegmentInfo::new(
            VERSION_LATEST.clone(),
            &segment_name,
            -1,
            Arc::clone(&directory_orig),
            false,
            Some(Arc::clone(&index_writer.config.codec)),
            HashMap::new(),
            random_id(),
            HashMap::new(),
            None,
        )?;
        let delete_slice = delete_queue.new_slice();
        let doc_state = DocState::new();
        // doc_state.similarity = Some(index_writer_config.similarity());
        Ok(DocumentsWriterPerThread {
            directory,
            directory_orig,
            doc_state,
            consumer: DefaultIndexingChain::default(),
            bytes_used: Counter::new(false),
            pending_updates: BufferedUpdates::new(segment_name),
            segment_info,
            aborted: false,
            num_docs_in_ram: 0,
            delete_queue,
            delete_slice,
            // this to init are just stub, the inner count should share with self.bytes_used
            byte_block_allocator: DirectTrackingAllocator::new(Counter::new(false)),
            int_block_allocator: Box::new(IntBlockAllocator::new(Counter::new(false))),
            pending_num_docs,
            index_writer_config,
            index_writer,
            files_to_delete: HashSet::new(),
            inited: false,
        })
    }

    pub fn init(&mut self, field_numbers: &mut FieldNumbers, codec: Arc<Codec>) {
        let field_infos = FieldInfosBuilder::new(FieldNumbersRef::new(field_numbers));
        self.byte_block_allocator =
            DirectTrackingAllocator::new(unsafe { self.bytes_used.shallow_copy() });
        self.int_block_allocator = Box::new(IntBlockAllocator::new(unsafe {
            self.bytes_used.shallow_copy()
        }));
        self.segment_info.set_codec(codec);
        let consumer = DefaultIndexingChain::new(self, field_infos);
        self.consumer = consumer;
        self.consumer.init();
        self.inited = true;
    }

    pub fn codec(&self) -> &Codec {
        self.index_writer_config.codec()
    }

    pub fn bytes_used(&self) -> i64 {
        self.bytes_used.get() // + self.pending_updates.bytes_used.get()
    }

    // Anything that will add N docs to the index should reserve first to make sure it's allowed
    fn reserve_one_doc(&mut self) -> Result<()> {
        self.pending_num_docs.fetch_add(1, Ordering::AcqRel);
        if self.pending_num_docs.load(Ordering::Acquire) > INDEX_MAX_DOCS as i64 {
            // Reserve failed: put the one doc back and throw exc:
            self.pending_num_docs.fetch_sub(1, Ordering::AcqRel);
            bail!(IllegalArgument(
                "number of documents in the index cannot exceed".into()
            ));
        }
        Ok(())
    }

    pub fn update_document(
        &mut self,
        doc: Vec<Box<Fieldable>>,
        del_term: Option<Term>,
    ) -> Result<u64> {
        // debug_assert!(self.inited);
        let mut doc = doc;
        self.reserve_one_doc()?;
        // self.doc_state.doc = doc;
        self.doc_state.doc_id = self.num_docs_in_ram as i32;
        {
            let mut doc_id_index = "";
            for f in &doc {
                if f.name() == "doc_id_index" {
                    doc_id_index = f.fields_data().unwrap().get_string().unwrap();
                }
            }
        }
        // self.doc_state.analyzer = analyzer;

        // Even on exception, the document is still added (but marked
        // deleted), so we don't need to un-reserve at that point.
        // Aborting exceptions will actually "lose" more than one
        // document, so the counter will be "wrong" in that case, but
        // it's very hard to fix (we can't easily distinguish aborting
        // vs non-aborting exceptions):
        let res = self
            .consumer
            .process_document(&mut self.doc_state, &mut doc);
        self.doc_state.clear();
        if !res.is_ok() {
            // mark document as deleted
            error!(" process document failed, res: {:?}", res);
            let doc = self.doc_state.doc_id;
            self.delete_doc_id(doc);
            self.num_docs_in_ram += 1;
            res?;
        }
        self.finish_document(del_term)
    }

    pub fn update_documents(
        &mut self,
        docs: Vec<Vec<Box<Fieldable>>>,
        del_term: Option<Term>,
    ) -> Result<u64> {
        // debug_assert!(self.inited);
        let mut doc_count = 0;
        let mut all_docs_indexed = false;

        let res = self.do_update_documents(docs, del_term, &mut doc_count, &mut all_docs_indexed);
        if !all_docs_indexed && !self.aborted {
            // the iterator threw an exception that is not aborting
            // go and mark all docs from this block as deleted
            let mut doc_id = self.num_docs_in_ram as i32 - 1;
            let end_doc_id = doc_id - doc_count;
            while doc_id > end_doc_id {
                self.delete_doc_id(doc_id);
                doc_id -= 1;
            }
        }
        self.doc_state.clear();
        res
    }

    fn do_update_documents(
        &mut self,
        docs: Vec<Vec<Box<Fieldable>>>,
        del_term: Option<Term>,
        doc_count: &mut i32,
        all_docs_indexed: &mut bool,
    ) -> Result<u64> {
        for mut doc in docs {
            // Even on exception, the document is still added (but marked
            // deleted), so we don't need to un-reserve at that point.
            // Aborting exceptions will actually "lose" more than one
            // document, so the counter will be "wrong" in that case, but
            // it's very hard to fix (we can't easily distinguish aborting
            // vs non-aborting exceptions):
            self.reserve_one_doc()?;
            // self.doc_state.doc = doc;
            self.doc_state.doc_id = self.num_docs_in_ram as i32;
            *doc_count += 1;

            let res = self
                .consumer
                .process_document(&mut self.doc_state, &mut doc);
            if res.is_err() {
                // Incr here because finishDocument will not
                // be called (because an exc is being thrown):
                self.num_docs_in_ram += 1;
                res?;
            }

            self.num_docs_in_ram += 1;
        }

        *all_docs_indexed = true;

        // Apply delTerm only after all indexing has
        // succeeded, but apply it only to docs prior to when
        // this batch started:
        let seq_no = if let Some(del_term) = del_term {
            let seq = self
                .delete_queue
                .add_term_to_slice(del_term, &mut self.delete_slice)?;
            self.delete_slice.apply(
                &mut self.pending_updates,
                self.num_docs_in_ram as i32 - *doc_count,
            );
            seq
        } else {
            let (seq, changed) = self.delete_queue.update_slice(&mut self.delete_slice);
            if changed {
                self.delete_slice.apply(
                    &mut self.pending_updates,
                    self.num_docs_in_ram as i32 - *doc_count,
                );
            } else {
                self.delete_slice.reset();
            }
            seq
        };
        Ok(seq_no)
    }

    // Buffer a specific docID for deletion. Currently only
    // used when we hit an exception when adding a document
    fn delete_doc_id(&mut self, doc_id_upto: DocId) {
        self.pending_updates.add_doc_id(doc_id_upto);
        // NOTE: we do not trigger flush here.  This is
        // potentially a RAM leak, if you have an app that tries
        // to add docs but every single doc always hits a
        // non-aborting exception.  Allowing a flush here gets
        // very messy because we are only invoked when handling
        // exceptions so to do this properly, while handling an
        // exception we'd have to go off and flush new deletes
        // which is risky (likely would hit some other
        // confounding exception).
    }

    fn finish_document(&mut self, del_term: Option<Term>) -> Result<u64> {
        // here we actually finish the document in two steps:
        // 1. push the delete into the queue and update our slice
        // 2. increment the DWPT private document id.
        //
        // the updated slice we get from 1. holds all the deletes that have
        // occurred since we updated the slice the last time.
        let mut apply_slice = self.num_docs_in_ram > 0;
        let seq_no: u64;
        if let Some(del_term) = del_term {
            seq_no = self
                .delete_queue
                .add_term_to_slice(del_term, &mut self.delete_slice)?

        // debug_assert!(self.delete_slice.is_tail_item(del_term));
        } else {
            let (seq, apply) = self.delete_queue.update_slice(&mut self.delete_slice);
            seq_no = seq;
            apply_slice = apply;
        }
        if apply_slice {
            self.delete_slice
                .apply(&mut self.pending_updates, self.num_docs_in_ram as i32);
        } else {
            self.delete_slice.reset();
        }
        self.num_docs_in_ram += 1;
        Ok(seq_no)
    }

    // Prepares this DWPT for flushing. This method will freeze and return the
    // `DocumentsWriterDeleteQueue`s global buffer and apply all pending deletes
    // to this DWPT
    pub fn prepare_flush(&mut self) -> Result<FrozenBufferUpdates> {
        debug_assert!(self.inited);
        debug_assert!(self.num_docs_in_ram > 0);

        let frozen_updates = self
            .delete_queue
            .freeze_global_buffer(Some(&mut self.delete_slice))?;
        // apply all deletes before we flush and release the delete slice
        self.delete_slice
            .apply(&mut self.pending_updates, self.num_docs_in_ram as i32);
        debug_assert!(self.delete_slice.is_empty());
        self.delete_slice.reset();
        Ok(frozen_updates)
    }

    /// Flush all pending docs to a new segment
    pub fn flush(&mut self) -> Result<Option<FlushedSegment>> {
        debug_assert!(self.inited);
        debug_assert!(self.num_docs_in_ram > 0);
        debug_assert!(self.delete_slice.is_empty());

        self.segment_info.max_doc = self.num_docs_in_ram as i32;
        let ctx = IOContext::Flush(FlushInfo::new(
            self.num_docs_in_ram,
            self.bytes_used() as u64,
        ));
        let mut flush_state = SegmentWriteState::new(
            Arc::clone(&self.directory),
            self.segment_info.clone(),
            self.consumer.field_infos.finish()?,
            Some(&self.pending_updates),
            ctx,
            "".into(),
        );
        let _start_mb_used = self.bytes_used() as f64 / 1024.0 / 1024.0;

        // Apply delete-by-docID now (delete-byDocID only
        // happens when an exception is hit processing that
        // doc, eg if analyzer has some problem w/ the text):
        if !self.pending_updates.deleted_doc_ids.is_empty() {
            flush_state.live_docs = self
                .codec()
                .live_docs_format()
                .new_live_docs(self.num_docs_in_ram as usize)?;
            for del_doc_id in self.pending_updates.deleted_doc_ids.as_ref() {
                flush_state
                    .live_docs
                    .as_bit_set_mut()
                    .clear(*del_doc_id as usize);
            }
            let docs_len = self.pending_updates.deleted_doc_ids.len();
            flush_state.del_count_on_flush = docs_len as u32;
            self.pending_updates.bytes_used.fetch_sub(
                docs_len * bufferd_updates::BYTES_PER_DEL_DOCID,
                Ordering::AcqRel,
            );
            self.pending_updates.deleted_doc_ids.clear();
        }

        if self.aborted {
            debug!("DWPT: flush: skip because aborting is set.");
            return Ok(None);
        }

        debug!(
            "DWPT: flush postings as segment '{}' num_docs={}",
            &flush_state.segment_info.name, self.num_docs_in_ram
        );
        let res = self.do_flush(flush_state);
        if res.is_err() {
            self.abort();
        }
        res
    }

    fn do_flush(&mut self, mut flush_state: SegmentWriteState) -> Result<Option<FlushedSegment>> {
        let t0 = SystemTime::now();
        let doc_writer = self as *mut DocumentsWriterPerThread;

        // re-init
        self.consumer.reset_doc_writer(doc_writer);
        self.consumer.init();

        self.consumer.flush(&mut flush_state)?;
        self.pending_updates.deleted_terms.clear();
        self.segment_info
            .set_files(&self.directory.create_files())?;
        let segment_info_per_commit = SegmentCommitInfo::new(
            self.segment_info.clone(),
            0,
            -1,
            -1,
            -1,
            HashMap::new(),
            HashSet::new(),
        );

        let mut fs = {
            let segment_deletes = if self.pending_updates.deleted_queries.is_empty() {
                self.pending_updates.clear();
                None
            } else {
                Some(&self.pending_updates)
            };
            FlushedSegment::new(
                Arc::new(segment_info_per_commit),
                flush_state.field_infos,
                segment_deletes,
                Arc::from(flush_state.live_docs),
                flush_state.del_count_on_flush,
            )
        };
        self.seal_flushed_segment(&mut fs)?;

        debug!(
            "DWPT: flush time {:?}",
            SystemTime::now().duration_since(t0).unwrap()
        );
        Ok(Some(fs))
    }

    fn seal_flushed_segment(&mut self, flushed_segment: &mut FlushedSegment) -> Result<()> {
        // set_diagnostics(&mut flushed_segment.segment_info.info, index_writer::SOURCE_FLUSH);

        let flush_info = FlushInfo::new(
            flushed_segment.segment_info.info.max_doc() as u32,
            flushed_segment.segment_info.size_in_bytes() as u64,
        );
        let ctx = &IOContext::Flush(flush_info);

        if self.index_writer_config.use_compound_file {
            // TODO: like addIndexes, we are relying on createCompoundFile to successfully
            // cleanup...
            let dir = TrackingDirectoryWrapper::new(&self.directory);
            let files = unsafe {
                // flushed_segment has no other reference, so Arc::get_mut is safe
                let segment_info = Arc::get_mut(&mut flushed_segment.segment_info).unwrap();
                (*self.index_writer).create_compound_file(&dir, &mut segment_info.info, ctx)?
            };
            self.files_to_delete.extend(files);
        }

        // Have codec write SegmentInfo.  Must do this after
        // creating CFS so that 1) .si isn't slurped into CFS,
        // and 2) .si reflects useCompoundFile=true change
        // above:
        {
            let segment_info = Arc::get_mut(&mut flushed_segment.segment_info).unwrap();
            let mut created_files = Vec::with_capacity(1);
            let res = self.codec().segment_info_format().write(
                self.directory.as_ref(),
                &mut segment_info.info,
                &mut created_files,
                ctx,
            );
            for f in created_files {
                segment_info.info.add_file(&f)?;
            }
            if res.is_err() {
                return res;
            }
        }

        // TODO: ideally we would freeze newSegment here!!
        // because any changes after writing the .si will be
        // lost...

        // Must write deleted docs after the CFS so we don't
        // slurp the del file into CFS:
        if !flushed_segment.live_docs.is_empty() {
            debug_assert!(flushed_segment.del_count > 0);

            // TODO: we should prune the segment if it's 100%
            // deleted... but merge will also catch it.

            // TODO: in the NRT case it'd be better to hand
            // this del vector over to the
            // shortly-to-be-opened SegmentReader and let it
            // carry the changes; there's no reason to use
            // filesystem as intermediary here.
            let codec = flushed_segment.segment_info.info.codec();
            codec.live_docs_format().write_live_docs(
                flushed_segment.live_docs.as_ref(),
                self.directory.as_ref(),
                flushed_segment.segment_info.as_ref(),
                flushed_segment.del_count as i32,
                ctx,
            )?;
            flushed_segment
                .segment_info
                .set_del_count(flushed_segment.del_count as i32)?;
            flushed_segment.segment_info.advance_del_gen();
        }
        Ok(())
    }

    /// Called if we hit an exception at a bad time (when
    /// updating the index files) and must discard all
    /// currently buffered docs.  This resets our state,
    /// discarding any docs added since last flush.
    pub fn abort(&mut self) {
        self.aborted = true;
        debug!("DWPT: now abort");

        if let Err(e) = self.consumer.abort() {
            error!("DefaultIndexChain abort failed by error: '{:?}'", e);
        }

        self.pending_updates.clear();
        debug!("DWPT: done abort");
    }
}

pub struct FlushedSegment {
    pub segment_info: Arc<SegmentCommitInfo>,
    pub field_infos: FieldInfos,
    pub segment_updates: Option<FrozenBufferUpdates>,
    pub live_docs: BitsRef,
    pub del_count: u32,
}

impl FlushedSegment {
    pub fn new(
        segment_info: Arc<SegmentCommitInfo>,
        field_infos: FieldInfos,
        buffered_updates: Option<&BufferedUpdates>,
        live_docs: BitsRef,
        del_count: u32,
    ) -> Self {
        let segment_updates = match buffered_updates {
            Some(b) if b.any() => Some(FrozenBufferUpdates::new(b, true)),
            _ => None,
        };
        FlushedSegment {
            segment_info,
            field_infos,
            segment_updates,
            live_docs,
            del_count,
        }
    }
}

/// `DocumentsWriterPerThreadPool` controls `ThreadState` instances
/// and their thread assignments during indexing. Each `ThreadState` holds
/// a reference to a `DocumentsWriterPerThread` that is once a
/// `ThreadState` is obtained from the pool exclusively used for indexing a
/// single document by the obtaining thread. Each indexing thread must obtain
/// such a `ThreadState` to make progress. Depending on the
/// `DocumentsWriterPerThreadPool` implementation `ThreadState`
/// assignments might differ from document to document.
///
/// Once a `DocumentsWriterPerThread` is selected for flush the thread pool
/// is reusing the flushing `DocumentsWriterPerThread`s ThreadState with a
/// new `DocumentsWriterPerThread` instance.
pub struct DocumentsWriterPerThreadPool {
    lock: Arc<Mutex<()>>,
    cond: Condvar,
    pub thread_states: Vec<Arc<Mutex<ThreadState>>>,
    free_list: Vec<usize>,
    // valid thread_state index in `self.thread_states`
    aborted: bool,
}

impl DocumentsWriterPerThreadPool {
    pub fn new() -> Self {
        DocumentsWriterPerThreadPool {
            lock: Arc::new(Mutex::new(())),
            cond: Condvar::new(),
            thread_states: vec![],
            free_list: vec![],
            aborted: false,
        }
    }

    /// Returns the active number of `ThreadState` instances.
    pub fn active_thread_state_count(&self) -> usize {
        let _l = self.lock.lock().unwrap();
        self.thread_states.len()
    }

    pub fn set_abort(&mut self) {
        let _l = self.lock.lock().unwrap();
        self.aborted = true;
    }

    fn clear_abort(&mut self) {
        let _l = self.lock.lock().unwrap();
        self.aborted = false;
        self.cond.notify_all();
    }

    /// Returns a new `ThreadState` iff any new state is available other `None`
    /// NOTE: the returned `ThreadState` is already locked iff non-None
    #[allow(needless_lifetimes)]
    fn new_thread_state(&mut self, lock: MutexGuard<()>) -> Result<LockedThreadState> {
        let mut l = lock;
        while self.aborted {
            l = self.cond.wait(l)?;
        }

        let thread_state = Arc::new(Mutex::new(ThreadState::new(None)));
        self.thread_states.push(thread_state);
        let idx = self.thread_states.len() - 1;

        Ok(LockedThreadState::new(
            Arc::clone(&self.thread_states[idx]),
            idx,
        ))
    }

    pub fn reset(&self, thread_state: &mut ThreadState) -> Option<DocumentsWriterPerThread> {
        thread_state.reset()
    }

    pub fn recycle(&self, _dwpt: DocumentsWriterPerThread) {
        // do nothing
    }

    /// this method is used by DocumentsWriter/FlushControl to obtain a ThreadState
    /// to do an indexing operation (add/update_document).
    pub fn get_and_lock(&mut self) -> Result<LockedThreadState> {
        let lock = Arc::clone(&self.lock);
        let l = lock.lock().unwrap();
        if let Some(mut idx) = self.free_list.pop() {
            if self.thread_states[idx].lock().unwrap().dwpt.is_none() {
                // This thread-state is not initialized, e.g. it
                // was just flushed. See if we can instead find
                // another free thread state that already has docs
                // indexed. This way if incoming thread concurrency
                // has decreased, we don't leave docs
                // indefinitely buffered, tying up RAM.  This
                // will instead get those thread states flushed,
                // freeing up RAM for larger segment flushes:
                for i in 0..self.free_list.len() {
                    let new_idx = self.free_list[i];
                    if self.thread_states[new_idx].lock().unwrap().dwpt.is_some() {
                        // Use this one instead, and swap it with
                        // the un-initialized one:
                        self.free_list[i] = idx;
                        idx = new_idx;
                        break;
                    }
                }
            }

            Ok(LockedThreadState::new(
                Arc::clone(&self.thread_states[idx]),
                idx,
            ))
        } else {
            self.new_thread_state(l)
        }
    }

    pub fn release(&mut self, state: LockedThreadState) {
        let lock = Arc::clone(&self.lock);
        let l = lock.lock().unwrap();
        debug_assert!(!self.free_list.contains(&state.index));
        self.free_list.push(state.index);
        // In case any thread is waiting, wake one of them up since we just
        // released a thread state; notify() should be sufficient but we do
        // notifyAll defensively:
        self.cond.notify_all();
    }

    pub fn locked_state(&self, idx: usize) -> LockedThreadState {
        debug_assert!(idx < self.thread_states.len());
        LockedThreadState::new(Arc::clone(&self.thread_states[idx]), idx)
    }
}

/// `ThreadState` references and guards a `DocumentsWriterPerThread`
/// instance that is used during indexing to build a in-memory index
/// segment. `ThreadState` also holds all flush related per-thread
/// data controlled by `DocumentsWriterFlushControl`.
///
/// A `ThreadState`, its methods and members should only accessed by one
/// thread a time. Users must acquire the lock via `ThreadState#lock()`
/// and release the lock in a finally block via `ThreadState#unlock()`
/// before accessing the state.
/// NOTE: this struct should always be used under a Mutex
pub struct ThreadState {
    pub dwpt: Option<DocumentsWriterPerThread>,
    // TODO this should really be part of DocumentsWriterFlushControl
    // write access guarded by DocumentsWriterFlushControl
    pub flush_pending: AtomicBool,
    // TODO this should really be part of DocumentsWriterFlushControl
    // write access guarded by DocumentsWriterFlushControl
    pub bytes_used: u64,
    // set by DocumentsWriter after each indexing op finishes
    last_seq_no: AtomicU64,
}

impl ThreadState {
    fn new(dwpt: Option<DocumentsWriterPerThread>) -> Self {
        ThreadState {
            dwpt,
            flush_pending: AtomicBool::new(false),
            bytes_used: 0,
            last_seq_no: AtomicU64::new(0),
        }
    }

    pub fn dwpt(&self) -> &DocumentsWriterPerThread {
        debug_assert!(self.dwpt.is_some());
        self.dwpt.as_ref().unwrap()
    }

    pub fn dwpt_mut(&mut self) -> &mut DocumentsWriterPerThread {
        debug_assert!(self.dwpt.is_some());
        self.dwpt.as_mut().unwrap()
    }

    pub fn flush_pending(&self) -> bool {
        self.flush_pending.load(Ordering::Acquire)
    }

    fn reset(&mut self) -> Option<DocumentsWriterPerThread> {
        let dwpt = mem::replace(&mut self.dwpt, None);
        self.bytes_used = 0;
        self.flush_pending.store(false, Ordering::Release);
        dwpt
    }

    pub fn inited(&self) -> bool {
        self.dwpt.is_some()
    }

    pub fn set_last_seq_no(&self, seq_no: u64) {
        self.last_seq_no.store(seq_no, Ordering::Release);
    }

    pub fn last_seq_no(&self) -> u64 {
        self.last_seq_no.load(Ordering::Acquire)
    }
}

pub struct LockedThreadState {
    pub state: Arc<Mutex<ThreadState>>,
    index: usize,
    // index of this state in  DocumentsWriterPerThreadPool.thread_states
}

impl LockedThreadState {
    pub fn new(state: Arc<Mutex<ThreadState>>, index: usize) -> Self {
        LockedThreadState { state, index }
    }
}

struct IntBlockAllocator {
    block_size: usize,
    pub bytes_used: Counter,
}

impl IntBlockAllocator {
    fn new(bytes_used: Counter) -> Self {
        IntBlockAllocator {
            block_size: INT_BLOCK_SIZE,
            bytes_used,
        }
    }
}

impl IntAllocator for IntBlockAllocator {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn recycle_int_blocks(&mut self, _blocks: &mut [Vec<i32>], _start: usize, end: usize) {
        self.bytes_used
            .add_get(-((end * self.block_size * 4) as i64));
    }

    fn int_block(&mut self) -> Vec<i32> {
        let b = vec![0; self.block_size];
        self.bytes_used.add_get((self.block_size * 4) as i64);
        b
    }

    fn shallow_copy(&mut self) -> Box<IntAllocator> {
        Box::new(IntBlockAllocator::new(unsafe {
            self.bytes_used.shallow_copy()
        }))
    }
}
