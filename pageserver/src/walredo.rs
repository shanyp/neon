//!
//! WAL redo. This service runs PostgreSQL in a special wal_redo mode
//! to apply given WAL records over an old page image and return new
//! page image.
//!
//! We rely on Postgres to perform WAL redo for us. We launch a
//! postgres process in special "wal redo" mode that's similar to
//! single-user mode. We then pass the previous page image, if any,
//! and all the WAL records we want to apply, to the postgres
//! process. Then we get the page image back. Communication with the
//! postgres process happens via stdin/stdout
//!
//! See pgxn/neon_walredo/walredoproc.c for the other side of
//! this communication.
//!
//! The Postgres process is assumed to be secure against malicious WAL
//! records. It achieves it by dropping privileges before replaying
//! any WAL records, so that even if an attacker hijacks the Postgres
//! process, he cannot escape out of it.
//!
use anyhow::Context;
use byteorder::{ByteOrder, LittleEndian};
use bytes::{BufMut, Bytes, BytesMut};
use nix::poll::*;
use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::io::prelude::*;
use std::ops::{Deref, DerefMut};
use std::os::unix::io::AsRawFd;
use std::os::unix::prelude::CommandExt;
use std::process::Stdio;
use std::process::{Child, ChildStdin, ChildStdout, Command};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::*;
use utils::{bin_ser::BeSer, id::TenantId, lsn::Lsn, nonblock::set_nonblock};

#[cfg(feature = "testing")]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::PageServerConf;
use crate::metrics::{
    WAL_REDO_BYTES_HISTOGRAM, WAL_REDO_RECORDS_HISTOGRAM, WAL_REDO_RECORD_COUNTER, WAL_REDO_TIME,
    WAL_REDO_WAIT_TIME,
};
use crate::pgdatadir_mapping::{key_to_rel_block, key_to_slru_block};
use crate::repository::Key;
use crate::walrecord::NeonWalRecord;
use pageserver_api::reltag::{RelTag, SlruKind};
use postgres_ffi::pg_constants;
use postgres_ffi::relfile_utils::VISIBILITYMAP_FORKNUM;
use postgres_ffi::v14::nonrelfile_utils::{
    mx_offset_to_flags_bitshift, mx_offset_to_flags_offset, mx_offset_to_member_offset,
    transaction_id_set_status,
};
use postgres_ffi::BLCKSZ;

///
/// `RelTag` + block number (`blknum`) gives us a unique id of the page in the cluster.
///
/// In Postgres `BufferTag` structure is used for exactly the same purpose.
/// [See more related comments here](https://github.com/postgres/postgres/blob/99c5852e20a0987eca1c38ba0c09329d4076b6a0/src/include/storage/buf_internals.h#L91).
///
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Serialize)]
pub(crate) struct BufferTag {
    pub rel: RelTag,
    pub blknum: u32,
}

struct ProcessInput {
    stdin: ChildStdin,
    n_requests: usize,
}

struct ProcessOutput {
    stdout: ChildStdout,
    pending_responses: VecDeque<Option<Bytes>>,
    n_processed_responses: usize,
}

///
/// This is the real implementation that uses a Postgres process to
/// perform WAL replay. Only one thread can use the process at a time,
/// that is controlled by the Mutex. In the future, we might want to
/// launch a pool of processes to allow concurrent replay of multiple
/// records.
///
pub struct PostgresRedoManager {
    tenant_id: TenantId,
    conf: &'static PageServerConf,
    redo_process: RwLock<Option<Arc<WalRedoProcess>>>,
}

/// Can this request be served by neon redo functions
/// or we need to pass it to wal-redo postgres process?
fn can_apply_in_neon(rec: &NeonWalRecord) -> bool {
    // Currently, we don't have bespoken Rust code to replay any
    // Postgres WAL records. But everything else is handled in neon.
    #[allow(clippy::match_like_matches_macro)]
    match rec {
        NeonWalRecord::Postgres {
            will_init: _,
            rec: _,
        } => false,
        _ => true,
    }
}

///
/// Public interface of WAL redo manager
///
impl PostgresRedoManager {
    ///
    /// Request the WAL redo manager to apply some WAL records
    ///
    /// The WAL redo is handled by a separate thread, so this just sends a request
    /// to the thread and waits for response.
    ///
    /// CANCEL SAFETY: NOT CANCEL SAFE.
    pub async fn request_redo(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<(Lsn, Bytes)>,
        records: Vec<(Lsn, NeonWalRecord)>,
        pg_version: u32,
    ) -> anyhow::Result<Bytes> {
        if records.is_empty() {
            anyhow::bail!("invalid WAL redo request with no records");
        }

        let base_img_lsn = base_img.as_ref().map(|p| p.0).unwrap_or(Lsn::INVALID);
        let mut img = base_img.map(|p| p.1);
        let mut batch_neon = can_apply_in_neon(&records[0].1);
        let mut batch_start = 0;
        for (i, record) in records.iter().enumerate().skip(1) {
            let rec_neon = can_apply_in_neon(&record.1);

            if rec_neon != batch_neon {
                let result = if batch_neon {
                    self.apply_batch_neon(key, lsn, img, &records[batch_start..i])
                } else {
                    self.apply_batch_postgres(
                        key,
                        lsn,
                        img,
                        base_img_lsn,
                        &records[batch_start..i],
                        self.conf.wal_redo_timeout,
                        pg_version,
                    )
                    .await
                };
                img = Some(result?);

                batch_neon = rec_neon;
                batch_start = i;
            }
        }
        // last batch
        if batch_neon {
            self.apply_batch_neon(key, lsn, img, &records[batch_start..])
        } else {
            self.apply_batch_postgres(
                key,
                lsn,
                img,
                base_img_lsn,
                &records[batch_start..],
                self.conf.wal_redo_timeout,
                pg_version,
            )
            .await
        }
    }
}

impl PostgresRedoManager {
    ///
    /// Create a new PostgresRedoManager.
    ///
    pub fn new(conf: &'static PageServerConf, tenant_id: TenantId) -> PostgresRedoManager {
        // The actual process is launched lazily, on first request.
        PostgresRedoManager {
            tenant_id,
            conf,
            redo_process: RwLock::new(None),
        }
    }

    ///
    /// Process one request for WAL redo using wal-redo postgres
    ///
    #[allow(clippy::too_many_arguments)]
    async fn apply_batch_postgres(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<Bytes>,
        base_img_lsn: Lsn,
        records: &[(Lsn, NeonWalRecord)],
        wal_redo_timeout: Duration,
        pg_version: u32,
    ) -> anyhow::Result<Bytes> {
        let (rel, blknum) = key_to_rel_block(key).context("invalid record")?;
        const MAX_RETRY_ATTEMPTS: u32 = 1;
        let start_time = Instant::now();
        let mut n_attempts = 0u32;
        loop {
            let lock_time = Instant::now();

            // launch the WAL redo process on first use
            let proc: Arc<WalRedoProcess> = {
                let proc_guard = self.redo_process.read().unwrap();
                match &*proc_guard {
                    None => {
                        // "upgrade" to write lock to launch the process
                        drop(proc_guard);
                        let mut proc_guard = self.redo_process.write().unwrap();
                        match &*proc_guard {
                            None => {
                                let proc = Arc::new(
                                    WalRedoProcess::launch(self.conf, self.tenant_id, pg_version)
                                        .context("launch walredo process")?,
                                );
                                *proc_guard = Some(Arc::clone(&proc));
                                proc
                            }
                            Some(proc) => Arc::clone(proc),
                        }
                    }
                    Some(proc) => Arc::clone(proc),
                }
            };

            WAL_REDO_WAIT_TIME.observe(lock_time.duration_since(start_time).as_secs_f64());

            // Relational WAL records are applied using wal-redo-postgres
            let buf_tag = BufferTag { rel, blknum };
            let result = proc
                .apply_wal_records(buf_tag, &base_img, records, wal_redo_timeout)
                .context("apply_wal_records");

            let end_time = Instant::now();
            let duration = end_time.duration_since(lock_time);

            let len = records.len();
            let nbytes = records.iter().fold(0, |acumulator, record| {
                acumulator
                    + match &record.1 {
                        NeonWalRecord::Postgres { rec, .. } => rec.len(),
                        _ => unreachable!("Only PostgreSQL records are accepted in this batch"),
                    }
            });

            WAL_REDO_TIME.observe(duration.as_secs_f64());
            WAL_REDO_RECORDS_HISTOGRAM.observe(len as f64);
            WAL_REDO_BYTES_HISTOGRAM.observe(nbytes as f64);

            debug!(
                "postgres applied {} WAL records ({} bytes) in {} us to reconstruct page image at LSN {}",
                len,
                nbytes,
                duration.as_micros(),
                lsn
            );

            // If something went wrong, don't try to reuse the process. Kill it, and
            // next request will launch a new one.
            if let Err(e) = result.as_ref() {
                error!(
                    "error applying {} WAL records {}..{} ({} bytes) to base image with LSN {} to reconstruct page image at LSN {} n_attempts={}: {:?}",
                    records.len(),
                    records.first().map(|p| p.0).unwrap_or(Lsn(0)),
                    records.last().map(|p| p.0).unwrap_or(Lsn(0)),
                    nbytes,
                    base_img_lsn,
                    lsn,
                    n_attempts,
                    e,
                );
                // Avoid concurrent callers hitting the same issue.
                // We can't prevent it from happening because we want to enable parallelism.
                {
                    let mut guard = self.redo_process.write().unwrap();
                    match &*guard {
                        Some(current_field_value) => {
                            if Arc::ptr_eq(current_field_value, &proc) {
                                // We're the first to observe an error from `proc`, it's our job to take it out of rotation.
                                *guard = None;
                            }
                        }
                        None => {
                            // Another thread was faster to observe the error, and already took the process out of rotation.
                        }
                    }
                }
                // NB: there may still be other concurrent threads using `proc`.
                // The last one will send SIGKILL when the underlying Arc reaches refcount 0.
                // NB: it's important to drop(proc) after drop(guard). Otherwise we'd keep
                // holding the lock while waiting for the process to exit.
                // NB: the drop impl blocks the current threads with a wait() system call for
                // the child process. We dropped the `guard` above so that other threads aren't
                // affected. But, it's good that the current thread _does_ block to wait.
                // If we instead deferred the waiting into the background / to tokio, it could
                // happen that if walredo always fails immediately, we spawn processes faster
                // than we can SIGKILL & `wait` for them to exit. By doing it the way we do here,
                // we limit this risk of run-away to at most $num_runtimes * $num_executor_threads.
                // This probably needs revisiting at some later point.
                let mut wait_done = proc.stderr_logger_task_done.clone();
                drop(proc);
                wait_done
                    .wait_for(|v| *v)
                    .await
                    .expect("we use scopeguard to ensure we always send `true` to the channel before dropping the sender");
            } else if n_attempts != 0 {
                info!(n_attempts, "retried walredo succeeded");
            }
            n_attempts += 1;
            if n_attempts > MAX_RETRY_ATTEMPTS || result.is_ok() {
                return result;
            }
        }
    }

    ///
    /// Process a batch of WAL records using bespoken Neon code.
    ///
    fn apply_batch_neon(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<Bytes>,
        records: &[(Lsn, NeonWalRecord)],
    ) -> anyhow::Result<Bytes> {
        let start_time = Instant::now();

        let mut page = BytesMut::new();
        if let Some(fpi) = base_img {
            // If full-page image is provided, then use it...
            page.extend_from_slice(&fpi[..]);
        } else {
            // All the current WAL record types that we can handle require a base image.
            anyhow::bail!("invalid neon WAL redo request with no base image");
        }

        // Apply all the WAL records in the batch
        for (record_lsn, record) in records.iter() {
            self.apply_record_neon(key, &mut page, *record_lsn, record)?;
        }
        // Success!
        let end_time = Instant::now();
        let duration = end_time.duration_since(start_time);
        WAL_REDO_TIME.observe(duration.as_secs_f64());

        debug!(
            "neon applied {} WAL records in {} ms to reconstruct page image at LSN {}",
            records.len(),
            duration.as_micros(),
            lsn
        );

        Ok(page.freeze())
    }

    fn apply_record_neon(
        &self,
        key: Key,
        page: &mut BytesMut,
        _record_lsn: Lsn,
        record: &NeonWalRecord,
    ) -> anyhow::Result<()> {
        match record {
            NeonWalRecord::Postgres {
                will_init: _,
                rec: _,
            } => {
                anyhow::bail!("tried to pass postgres wal record to neon WAL redo");
            }
            NeonWalRecord::ClearVisibilityMapFlags {
                new_heap_blkno,
                old_heap_blkno,
                flags,
            } => {
                // sanity check that this is modifying the correct relation
                let (rel, blknum) = key_to_rel_block(key).context("invalid record")?;
                assert!(
                    rel.forknum == VISIBILITYMAP_FORKNUM,
                    "ClearVisibilityMapFlags record on unexpected rel {}",
                    rel
                );
                if let Some(heap_blkno) = *new_heap_blkno {
                    // Calculate the VM block and offset that corresponds to the heap block.
                    let map_block = pg_constants::HEAPBLK_TO_MAPBLOCK(heap_blkno);
                    let map_byte = pg_constants::HEAPBLK_TO_MAPBYTE(heap_blkno);
                    let map_offset = pg_constants::HEAPBLK_TO_OFFSET(heap_blkno);

                    // Check that we're modifying the correct VM block.
                    assert!(map_block == blknum);

                    // equivalent to PageGetContents(page)
                    let map = &mut page[pg_constants::MAXALIGN_SIZE_OF_PAGE_HEADER_DATA..];

                    map[map_byte as usize] &= !(flags << map_offset);
                }

                // Repeat for 'old_heap_blkno', if any
                if let Some(heap_blkno) = *old_heap_blkno {
                    let map_block = pg_constants::HEAPBLK_TO_MAPBLOCK(heap_blkno);
                    let map_byte = pg_constants::HEAPBLK_TO_MAPBYTE(heap_blkno);
                    let map_offset = pg_constants::HEAPBLK_TO_OFFSET(heap_blkno);

                    assert!(map_block == blknum);

                    let map = &mut page[pg_constants::MAXALIGN_SIZE_OF_PAGE_HEADER_DATA..];

                    map[map_byte as usize] &= !(flags << map_offset);
                }
            }
            // Non-relational WAL records are handled here, with custom code that has the
            // same effects as the corresponding Postgres WAL redo function.
            NeonWalRecord::ClogSetCommitted { xids, timestamp } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).context("invalid record")?;
                assert_eq!(
                    slru_kind,
                    SlruKind::Clog,
                    "ClogSetCommitted record with unexpected key {}",
                    key
                );
                for &xid in xids {
                    let pageno = xid / pg_constants::CLOG_XACTS_PER_PAGE;
                    let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                    let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;

                    // Check that we're modifying the correct CLOG block.
                    assert!(
                        segno == expected_segno,
                        "ClogSetCommitted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );
                    assert!(
                        blknum == expected_blknum,
                        "ClogSetCommitted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );

                    transaction_id_set_status(
                        xid,
                        pg_constants::TRANSACTION_STATUS_COMMITTED,
                        page,
                    );
                }

                // Append the timestamp
                if page.len() == BLCKSZ as usize + 8 {
                    page.truncate(BLCKSZ as usize);
                }
                if page.len() == BLCKSZ as usize {
                    page.extend_from_slice(&timestamp.to_be_bytes());
                } else {
                    warn!(
                        "CLOG blk {} in seg {} has invalid size {}",
                        blknum,
                        segno,
                        page.len()
                    );
                }
            }
            NeonWalRecord::ClogSetAborted { xids } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).context("invalid record")?;
                assert_eq!(
                    slru_kind,
                    SlruKind::Clog,
                    "ClogSetAborted record with unexpected key {}",
                    key
                );
                for &xid in xids {
                    let pageno = xid / pg_constants::CLOG_XACTS_PER_PAGE;
                    let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                    let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;

                    // Check that we're modifying the correct CLOG block.
                    assert!(
                        segno == expected_segno,
                        "ClogSetAborted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );
                    assert!(
                        blknum == expected_blknum,
                        "ClogSetAborted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );

                    transaction_id_set_status(xid, pg_constants::TRANSACTION_STATUS_ABORTED, page);
                }
            }
            NeonWalRecord::MultixactOffsetCreate { mid, moff } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).context("invalid record")?;
                assert_eq!(
                    slru_kind,
                    SlruKind::MultiXactOffsets,
                    "MultixactOffsetCreate record with unexpected key {}",
                    key
                );
                // Compute the block and offset to modify.
                // See RecordNewMultiXact in PostgreSQL sources.
                let pageno = mid / pg_constants::MULTIXACT_OFFSETS_PER_PAGE as u32;
                let entryno = mid % pg_constants::MULTIXACT_OFFSETS_PER_PAGE as u32;
                let offset = (entryno * 4) as usize;

                // Check that we're modifying the correct multixact-offsets block.
                let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;
                assert!(
                    segno == expected_segno,
                    "MultiXactOffsetsCreate record for multi-xid {} with unexpected key {}",
                    mid,
                    key
                );
                assert!(
                    blknum == expected_blknum,
                    "MultiXactOffsetsCreate record for multi-xid {} with unexpected key {}",
                    mid,
                    key
                );

                LittleEndian::write_u32(&mut page[offset..offset + 4], *moff);
            }
            NeonWalRecord::MultixactMembersCreate { moff, members } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).context("invalid record")?;
                assert_eq!(
                    slru_kind,
                    SlruKind::MultiXactMembers,
                    "MultixactMembersCreate record with unexpected key {}",
                    key
                );
                for (i, member) in members.iter().enumerate() {
                    let offset = moff + i as u32;

                    // Compute the block and offset to modify.
                    // See RecordNewMultiXact in PostgreSQL sources.
                    let pageno = offset / pg_constants::MULTIXACT_MEMBERS_PER_PAGE as u32;
                    let memberoff = mx_offset_to_member_offset(offset);
                    let flagsoff = mx_offset_to_flags_offset(offset);
                    let bshift = mx_offset_to_flags_bitshift(offset);

                    // Check that we're modifying the correct multixact-members block.
                    let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                    let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;
                    assert!(
                        segno == expected_segno,
                        "MultiXactMembersCreate record for offset {} with unexpected key {}",
                        moff,
                        key
                    );
                    assert!(
                        blknum == expected_blknum,
                        "MultiXactMembersCreate record for offset {} with unexpected key {}",
                        moff,
                        key
                    );

                    let mut flagsval = LittleEndian::read_u32(&page[flagsoff..flagsoff + 4]);
                    flagsval &= !(((1 << pg_constants::MXACT_MEMBER_BITS_PER_XACT) - 1) << bshift);
                    flagsval |= member.status << bshift;
                    LittleEndian::write_u32(&mut page[flagsoff..flagsoff + 4], flagsval);
                    LittleEndian::write_u32(&mut page[memberoff..memberoff + 4], member.xid);
                }
            }
        }

        Ok(())
    }
}

///
/// Command with ability not to give all file descriptors to child process
///
trait CloseFileDescriptors: CommandExt {
    ///
    /// Close file descriptors (other than stdin, stdout, stderr) in child process
    ///
    fn close_fds(&mut self) -> &mut Command;
}

impl<C: CommandExt> CloseFileDescriptors for C {
    fn close_fds(&mut self) -> &mut Command {
        unsafe {
            self.pre_exec(move || {
                // SAFETY: Code executed inside pre_exec should have async-signal-safety,
                // which means it should be safe to execute inside a signal handler.
                // The precise meaning depends on platform. See `man signal-safety`
                // for the linux definition.
                //
                // The set_fds_cloexec_threadsafe function is documented to be
                // async-signal-safe.
                //
                // Aside from this function, the rest of the code is re-entrant and
                // doesn't make any syscalls. We're just passing constants.
                //
                // NOTE: It's easy to indirectly cause a malloc or lock a mutex,
                // which is not async-signal-safe. Be careful.
                close_fds::set_fds_cloexec_threadsafe(3, &[]);
                Ok(())
            })
        }
    }
}

struct WalRedoProcess {
    #[allow(dead_code)]
    conf: &'static PageServerConf,
    tenant_id: TenantId,
    // Some() on construction, only becomes None on Drop.
    child: Option<NoLeakChild>,
    stdout: Mutex<ProcessOutput>,
    stdin: Mutex<ProcessInput>,
    stderr_logger_cancel: CancellationToken,
    stderr_logger_task_done: tokio::sync::watch::Receiver<bool>,
    /// Counter to separate same sized walredo inputs failing at the same millisecond.
    #[cfg(feature = "testing")]
    dump_sequence: AtomicUsize,
}

impl WalRedoProcess {
    //
    // Start postgres binary in special WAL redo mode.
    //
    #[instrument(skip_all,fields(tenant_id=%tenant_id, pg_version=pg_version))]
    fn launch(
        conf: &'static PageServerConf,
        tenant_id: TenantId,
        pg_version: u32,
    ) -> anyhow::Result<Self> {
        let pg_bin_dir_path = conf.pg_bin_dir(pg_version).context("pg_bin_dir")?; // TODO these should be infallible.
        let pg_lib_dir_path = conf.pg_lib_dir(pg_version).context("pg_lib_dir")?;

        // Start postgres itself
        let child = Command::new(pg_bin_dir_path.join("postgres"))
            .arg("--wal-redo")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .env_clear()
            .env("LD_LIBRARY_PATH", &pg_lib_dir_path)
            .env("DYLD_LIBRARY_PATH", &pg_lib_dir_path)
            // The redo process is not trusted, and runs in seccomp mode that
            // doesn't allow it to open any files. We have to also make sure it
            // doesn't inherit any file descriptors from the pageserver, that
            // would allow an attacker to read any files that happen to be open
            // in the pageserver.
            //
            // The Rust standard library makes sure to mark any file descriptors with
            // as close-on-exec by default, but that's not enough, since we use
            // libraries that directly call libc open without setting that flag.
            .close_fds()
            .spawn_no_leak_child(tenant_id)
            .context("spawn process")?;

        let mut child = scopeguard::guard(child, |child| {
            error!("killing wal-redo-postgres process due to a problem during launch");
            child.kill_and_wait();
        });

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        macro_rules! set_nonblock_or_log_err {
            ($file:ident) => {{
                let res = set_nonblock($file.as_raw_fd());
                if let Err(e) = &res {
                    error!(error = %e, file = stringify!($file), pid = child.id(), "set_nonblock failed");
                }
                res
            }};
        }
        set_nonblock_or_log_err!(stdin)?;
        set_nonblock_or_log_err!(stdout)?;
        set_nonblock_or_log_err!(stderr)?;

        let mut stderr = tokio::io::unix::AsyncFd::new(stderr).context("AsyncFd::with_interest")?;

        // all fallible operations post-spawn are complete, so get rid of the guard
        let child = scopeguard::ScopeGuard::into_inner(child);

        let stderr_logger_cancel = CancellationToken::new();
        let (stderr_logger_task_done_tx, stderr_logger_task_done_rx) =
            tokio::sync::watch::channel(false);
        tokio::spawn({
            let stderr_logger_cancel = stderr_logger_cancel.clone();
            async move {
                scopeguard::defer! {
                    debug!("wal-redo-postgres stderr_logger_task finished");
                    let _ = stderr_logger_task_done_tx.send(true);
                }
                debug!("wal-redo-postgres stderr_logger_task started");
                loop {
                    // NB: we purposefully don't do a select! for the cancellation here.
                    // The cancellation would likely cause us to miss stderr messages.
                    // We can rely on this to return from .await because when we SIGKILL
                    // the child, the writing end of the stderr pipe gets closed.
                    match stderr.readable_mut().await {
                        Ok(mut guard) => {
                            let mut errbuf = [0; 16384];
                            let res = guard.try_io(|fd| {
                                use std::io::Read;
                                fd.get_mut().read(&mut errbuf)
                            });
                            match res {
                                Ok(Ok(0)) => {
                                    // it closed the stderr pipe
                                    break;
                                }
                                Ok(Ok(n)) => {
                                    // The message might not be split correctly into lines here. But this is
                                    // good enough, the important thing is to get the message to the log.
                                    let output = String::from_utf8_lossy(&errbuf[0..n]).to_string();
                                    error!(output, "received output");
                                },
                                Ok(Err(e)) => {
                                    error!(error = ?e, "read() error, waiting for cancellation");
                                    stderr_logger_cancel.cancelled().await;
                                    error!(error = ?e, "read() error, cancellation complete");
                                    break;
                                }
                                Err(e) => {
                                    let _e: tokio::io::unix::TryIoError = e;
                                    // the read() returned WouldBlock, that's expected
                                }
                            }
                        }
                        Err(e) => {
                            error!(error = ?e, "read() error, waiting for cancellation");
                            stderr_logger_cancel.cancelled().await;
                            error!(error = ?e, "read() error, cancellation complete");
                            break;
                        }
                    }
                }
            }.instrument(tracing::info_span!(parent: None, "wal-redo-postgres-stderr", pid = child.id(), tenant_id = %tenant_id, %pg_version))
        });

        Ok(Self {
            conf,
            tenant_id,
            child: Some(child),
            stdin: Mutex::new(ProcessInput {
                stdin,
                n_requests: 0,
            }),
            stdout: Mutex::new(ProcessOutput {
                stdout,
                pending_responses: VecDeque::new(),
                n_processed_responses: 0,
            }),
            stderr_logger_cancel,
            stderr_logger_task_done: stderr_logger_task_done_rx,
            #[cfg(feature = "testing")]
            dump_sequence: AtomicUsize::default(),
        })
    }

    fn id(&self) -> u32 {
        self.child
            .as_ref()
            .expect("must not call this during Drop")
            .id()
    }

    // Apply given WAL records ('records') over an old page image. Returns
    // new page image.
    //
    #[instrument(skip_all, fields(tenant_id=%self.tenant_id, pid=%self.id()))]
    fn apply_wal_records(
        &self,
        tag: BufferTag,
        base_img: &Option<Bytes>,
        records: &[(Lsn, NeonWalRecord)],
        wal_redo_timeout: Duration,
    ) -> anyhow::Result<Bytes> {
        let input = self.stdin.lock().unwrap();

        // Serialize all the messages to send the WAL redo process first.
        //
        // This could be problematic if there are millions of records to replay,
        // but in practice the number of records is usually so small that it doesn't
        // matter, and it's better to keep this code simple.
        //
        // Most requests start with a before-image with BLCKSZ bytes, followed by
        // by some other WAL records. Start with a buffer that can hold that
        // comfortably.
        let mut writebuf: Vec<u8> = Vec::with_capacity((BLCKSZ as usize) * 3);
        build_begin_redo_for_block_msg(tag, &mut writebuf);
        if let Some(img) = base_img {
            build_push_page_msg(tag, img, &mut writebuf);
        }
        for (lsn, rec) in records.iter() {
            if let NeonWalRecord::Postgres {
                will_init: _,
                rec: postgres_rec,
            } = rec
            {
                build_apply_record_msg(*lsn, postgres_rec, &mut writebuf);
            } else {
                anyhow::bail!("tried to pass neon wal record to postgres WAL redo");
            }
        }
        build_get_page_msg(tag, &mut writebuf);
        WAL_REDO_RECORD_COUNTER.inc_by(records.len() as u64);

        let res = self.apply_wal_records0(&writebuf, input, wal_redo_timeout);

        if res.is_err() {
            // not all of these can be caused by this particular input, however these are so rare
            // in tests so capture all.
            self.record_and_log(&writebuf);
        }

        res
    }

    fn apply_wal_records0(
        &self,
        writebuf: &[u8],
        input: MutexGuard<ProcessInput>,
        wal_redo_timeout: Duration,
    ) -> anyhow::Result<Bytes> {
        let mut proc = { input }; // TODO: remove this legacy rename, but this keep the patch small.
        let mut nwrite = 0usize;

        let mut stdin_pollfds = [PollFd::new(proc.stdin.as_raw_fd(), PollFlags::POLLOUT)];

        while nwrite < writebuf.len() {
            let n = loop {
                match nix::poll::poll(&mut stdin_pollfds[..], wal_redo_timeout.as_millis() as i32) {
                    Err(nix::errno::Errno::EINTR) => continue,
                    res => break res,
                }
            }?;

            if n == 0 {
                anyhow::bail!("WAL redo timed out");
            }

            // If 'stdin' is writeable, do write.
            let in_revents = stdin_pollfds[0].revents().unwrap();
            if in_revents & (PollFlags::POLLERR | PollFlags::POLLOUT) != PollFlags::empty() {
                nwrite += proc.stdin.write(&writebuf[nwrite..])?;
            } else if in_revents.contains(PollFlags::POLLHUP) {
                // We still have more data to write, but the process closed the pipe.
                anyhow::bail!("WAL redo process closed its stdin unexpectedly");
            }
        }
        let request_no = proc.n_requests;
        proc.n_requests += 1;
        drop(proc);

        // To improve walredo performance we separate sending requests and receiving
        // responses. Them are protected by different mutexes (output and input).
        // If thread T1, T2, T3 send requests D1, D2, D3 to walredo process
        // then there is not warranty that T1 will first granted output mutex lock.
        // To address this issue we maintain number of sent requests, number of processed
        // responses and ring buffer with pending responses. After sending response
        // (under input mutex), threads remembers request number. Then it releases
        // input mutex, locks output mutex and fetch in ring buffer all responses until
        // its stored request number. The it takes correspondent element from
        // pending responses ring buffer and truncate all empty elements from the front,
        // advancing processed responses number.

        let mut output = self.stdout.lock().unwrap();
        let mut stdout_pollfds = [PollFd::new(output.stdout.as_raw_fd(), PollFlags::POLLIN)];
        let n_processed_responses = output.n_processed_responses;
        while n_processed_responses + output.pending_responses.len() <= request_no {
            // We expect the WAL redo process to respond with an 8k page image. We read it
            // into this buffer.
            let mut resultbuf = vec![0; BLCKSZ.into()];
            let mut nresult: usize = 0; // # of bytes read into 'resultbuf' so far
            while nresult < BLCKSZ.into() {
                // We do two things simultaneously: reading response from stdout
                // and forward any logging information that the child writes to its stderr to the page server's log.
                let n = loop {
                    match nix::poll::poll(
                        &mut stdout_pollfds[..],
                        wal_redo_timeout.as_millis() as i32,
                    ) {
                        Err(nix::errno::Errno::EINTR) => continue,
                        res => break res,
                    }
                }?;

                if n == 0 {
                    anyhow::bail!("WAL redo timed out");
                }

                // If we have some data in stdout, read it to the result buffer.
                let out_revents = stdout_pollfds[0].revents().unwrap();
                if out_revents & (PollFlags::POLLERR | PollFlags::POLLIN) != PollFlags::empty() {
                    nresult += output.stdout.read(&mut resultbuf[nresult..])?;
                } else if out_revents.contains(PollFlags::POLLHUP) {
                    anyhow::bail!("WAL redo process closed its stdout unexpectedly");
                }
            }
            output
                .pending_responses
                .push_back(Some(Bytes::from(resultbuf)));
        }
        // Replace our request's response with None in `pending_responses`.
        // Then make space in the ring buffer by clearing out any seqence of contiguous
        // `None`'s from the front of `pending_responses`.
        // NB: We can't pop_front() because other requests' responses because another
        // requester might have grabbed the output mutex before us:
        // T1: grab input mutex
        // T1: send request_no 23
        // T1: release input mutex
        // T2: grab input mutex
        // T2: send request_no 24
        // T2: release input mutex
        // T2: grab output mutex
        // T2: n_processed_responses + output.pending_responses.len() <= request_no
        //            23                                0                   24
        // T2: enters poll loop that reads stdout
        // T2: put response for 23 into pending_responses
        // T2: put response for 24 into pending_resposnes
        // pending_responses now looks like this: Front Some(response_23) Some(response_24) Back
        // T2: takes its response_24
        // pending_responses now looks like this: Front Some(response_23) None Back
        // T2: does the while loop below
        // pending_responses now looks like this: Front Some(response_23) None Back
        // T2: releases output mutex
        // T1: grabs output mutex
        // T1: n_processed_responses + output.pending_responses.len() > request_no
        //            23                                2                   23
        // T1: skips poll loop that reads stdout
        // T1: takes its response_23
        // pending_responses now looks like this: Front None None Back
        // T2: does the while loop below
        // pending_responses now looks like this: Front Back
        // n_processed_responses now has value 25
        let res = output.pending_responses[request_no - n_processed_responses]
            .take()
            .expect("we own this request_no, nobody else is supposed to take it");
        while let Some(front) = output.pending_responses.front() {
            if front.is_none() {
                output.pending_responses.pop_front();
                output.n_processed_responses += 1;
            } else {
                break;
            }
        }
        Ok(res)
    }

    #[cfg(feature = "testing")]
    fn record_and_log(&self, writebuf: &[u8]) {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let seq = self.dump_sequence.fetch_add(1, Ordering::Relaxed);

        // these files will be collected to an allure report
        let filename = format!("walredo-{millis}-{}-{seq}.walredo", writebuf.len());

        let path = self.conf.tenant_path(&self.tenant_id).join(&filename);

        let res = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .read(true)
            .open(path)
            .and_then(|mut f| f.write_all(writebuf));

        // trip up allowed_errors
        if let Err(e) = res {
            tracing::error!(target=%filename, length=writebuf.len(), "failed to write out the walredo errored input: {e}");
        } else {
            tracing::error!(filename, "erroring walredo input saved");
        }
    }

    #[cfg(not(feature = "testing"))]
    fn record_and_log(&self, _: &[u8]) {}
}

impl Drop for WalRedoProcess {
    fn drop(&mut self) {
        self.child
            .take()
            .expect("we only do this once")
            .kill_and_wait();
        self.stderr_logger_cancel.cancel();
        // no way to wait for stderr_logger_task from Drop because that is async only
    }
}

/// Wrapper type around `std::process::Child` which guarantees that the child
/// will be killed and waited-for by this process before being dropped.
struct NoLeakChild {
    tenant_id: TenantId,
    child: Option<Child>,
}

impl Deref for NoLeakChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        self.child.as_ref().expect("must not use from drop")
    }
}

impl DerefMut for NoLeakChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child.as_mut().expect("must not use from drop")
    }
}

impl NoLeakChild {
    fn spawn(tenant_id: TenantId, command: &mut Command) -> io::Result<Self> {
        let child = command.spawn()?;
        Ok(NoLeakChild {
            tenant_id,
            child: Some(child),
        })
    }

    fn kill_and_wait(mut self) {
        let child = match self.child.take() {
            Some(child) => child,
            None => return,
        };
        Self::kill_and_wait_impl(child);
    }

    #[instrument(skip_all, fields(pid=child.id()))]
    fn kill_and_wait_impl(mut child: Child) {
        let res = child.kill();
        if let Err(e) = res {
            // This branch is very unlikely because:
            // - We (= pageserver) spawned this process successfully, so, we're allowed to kill it.
            // - This is the only place that calls .kill()
            // - We consume `self`, so, .kill() can't be called twice.
            // - If the process exited by itself or was killed by someone else,
            //   .kill() will still succeed because we haven't wait()'ed yet.
            //
            // So, if we arrive here, we have really no idea what happened,
            // whether the PID stored in self.child is still valid, etc.
            // If this function were fallible, we'd return an error, but
            // since it isn't, all we can do is log an error and proceed
            // with the wait().
            error!(error = %e, "failed to SIGKILL; subsequent wait() might fail or wait for wrong process");
        }

        match child.wait() {
            Ok(exit_status) => {
                info!(exit_status = %exit_status, "wait successful");
            }
            Err(e) => {
                error!(error = %e, "wait error; might leak the child process; it will show as zombie (defunct)");
            }
        }
    }
}

impl Drop for NoLeakChild {
    fn drop(&mut self) {
        let child = match self.child.take() {
            Some(child) => child,
            None => return,
        };
        let tenant_id = self.tenant_id;
        // Offload the kill+wait of the child process into the background.
        // If someone stops the runtime, we'll leak the child process.
        // We can ignore that case because we only stop the runtime on pageserver exit.
        tokio::runtime::Handle::current().spawn(async move {
            tokio::task::spawn_blocking(move || {
                // Intentionally don't inherit the tracing context from whoever is dropping us.
                // This thread here is going to outlive of our dropper.
                let span = tracing::info_span!("walredo", %tenant_id);
                let _entered = span.enter();
                Self::kill_and_wait_impl(child);
            })
            .await
        });
    }
}

trait NoLeakChildCommandExt {
    fn spawn_no_leak_child(&mut self, tenant_id: TenantId) -> io::Result<NoLeakChild>;
}

impl NoLeakChildCommandExt for Command {
    fn spawn_no_leak_child(&mut self, tenant_id: TenantId) -> io::Result<NoLeakChild> {
        NoLeakChild::spawn(tenant_id, self)
    }
}

// Functions for constructing messages to send to the postgres WAL redo
// process. See pgxn/neon_walredo/walredoproc.c for
// explanation of the protocol.

fn build_begin_redo_for_block_msg(tag: BufferTag, buf: &mut Vec<u8>) {
    let len = 4 + 1 + 4 * 4;

    buf.put_u8(b'B');
    buf.put_u32(len as u32);

    tag.ser_into(buf)
        .expect("serialize BufferTag should always succeed");
}

fn build_push_page_msg(tag: BufferTag, base_img: &[u8], buf: &mut Vec<u8>) {
    assert!(base_img.len() == 8192);

    let len = 4 + 1 + 4 * 4 + base_img.len();

    buf.put_u8(b'P');
    buf.put_u32(len as u32);
    tag.ser_into(buf)
        .expect("serialize BufferTag should always succeed");
    buf.put(base_img);
}

fn build_apply_record_msg(endlsn: Lsn, rec: &[u8], buf: &mut Vec<u8>) {
    let len = 4 + 8 + rec.len();

    buf.put_u8(b'A');
    buf.put_u32(len as u32);
    buf.put_u64(endlsn.0);
    buf.put(rec);
}

fn build_get_page_msg(tag: BufferTag, buf: &mut Vec<u8>) {
    let len = 4 + 1 + 4 * 4;

    buf.put_u8(b'G');
    buf.put_u32(len as u32);
    tag.ser_into(buf)
        .expect("serialize BufferTag should always succeed");
}

#[cfg(test)]
mod tests {
    use super::PostgresRedoManager;
    use crate::repository::Key;
    use crate::{config::PageServerConf, walrecord::NeonWalRecord};
    use bytes::Bytes;
    use std::str::FromStr;
    use utils::{id::TenantId, lsn::Lsn};

    #[tokio::test]
    async fn short_v14_redo() {
        let expected = std::fs::read("fixtures/short_v14_redo.page").unwrap();

        let h = RedoHarness::new().unwrap();

        let page = h
            .manager
            .request_redo(
                Key {
                    field1: 0,
                    field2: 1663,
                    field3: 13010,
                    field4: 1259,
                    field5: 0,
                    field6: 0,
                },
                Lsn::from_str("0/16E2408").unwrap(),
                None,
                short_records(),
                14,
            )
            .await
            .unwrap();

        assert_eq!(&expected, &*page);
    }

    #[tokio::test]
    async fn short_v14_fails_for_wrong_key_but_returns_zero_page() {
        let h = RedoHarness::new().unwrap();

        let page = h
            .manager
            .request_redo(
                Key {
                    field1: 0,
                    field2: 1663,
                    // key should be 13010
                    field3: 13130,
                    field4: 1259,
                    field5: 0,
                    field6: 0,
                },
                Lsn::from_str("0/16E2408").unwrap(),
                None,
                short_records(),
                14,
            )
            .await
            .unwrap();

        // TODO: there will be some stderr printout, which is forwarded to tracing that could
        // perhaps be captured as long as it's in the same thread.
        assert_eq!(page, crate::ZERO_PAGE);
    }

    #[tokio::test]
    async fn test_stderr() {
        let h = RedoHarness::new().unwrap();
        h
            .manager
            .request_redo(
                Key::from_i128(0),
                Lsn::INVALID,
                None,
                short_records(),
                16, /* 16 currently produces stderr output on startup, which adds a nice extra edge */
            )
            .await
            .unwrap_err();
    }

    #[allow(clippy::octal_escapes)]
    fn short_records() -> Vec<(Lsn, NeonWalRecord)> {
        vec![
            (
                Lsn::from_str("0/16A9388").unwrap(),
                NeonWalRecord::Postgres {
                    will_init: true,
                    rec: Bytes::from_static(b"j\x03\0\0\0\x04\0\0\xe8\x7fj\x01\0\0\0\0\0\n\0\0\xd0\x16\x13Y\0\x10\0\04\x03\xd4\0\x05\x7f\x06\0\0\xd22\0\0\xeb\x04\0\0\0\0\0\0\xff\x03\0\0\0\0\x80\xeca\x01\0\0\x01\0\xd4\0\xa0\x1d\0 \x04 \0\0\0\0/\0\x01\0\xa0\x9dX\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0.\0\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\00\x9f\x9a\x01P\x9e\xb2\x01\0\x04\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x02\0!\0\x01\x08 \xff\xff\xff?\0\0\0\0\0\0@\0\0another_table\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x98\x08\0\0\x02@\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0@\0\0\0\0\0\0\0\0\0\0\0\0\x80\xbf\0\0\0\0\0\0\0\0\0\0pr\x01\0\0\0\0\0\0\0\0\x01d\0\0\0\0\0\0\x04\0\0\x01\0\0\0\0\0\0\0\x0c\x02\0\0\0\0\0\0\0\0\0\0\0\0\0\0/\0!\x80\x03+ \xff\xff\xff\x7f\0\0\0\0\0\xdf\x04\0\0pg_type\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x0b\0\0\0G\0\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0\0\0\0\0\0\0\0\x0e\0\0\0\0@\x16D\x0e\0\0\0K\x10\0\0\x01\0pr \0\0\0\0\0\0\0\0\x01n\0\0\0\0\0\xd6\x02\0\0\x01\0\0\0[\x01\0\0\0\0\0\0\0\t\x04\0\0\x02\0\0\0\x01\0\0\0\n\0\0\0\n\0\0\0\x7f\0\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0\0\0C\x01\0\0\x15\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0.\0!\x80\x03+ \xff\xff\xff\x7f\0\0\0\0\0;\n\0\0pg_statistic\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x0b\0\0\0\xfd.\0\0\0\0\0\0\n\0\0\0\x02\0\0\0;\n\0\0\0\0\0\0\x13\0\0\0\0\0\xcbC\x13\0\0\0\x18\x0b\0\0\x01\0pr\x1f\0\0\0\0\0\0\0\0\x01n\0\0\0\0\0\xd6\x02\0\0\x01\0\0\0C\x01\0\0\0\0\0\0\0\t\x04\0\0\x01\0\0\0\x01\0\0\0\n\0\0\0\n\0\0\0\x7f\0\0\0\0\0\0\x02\0\x01")
                }
            ),
            (
                Lsn::from_str("0/16D4080").unwrap(),
                NeonWalRecord::Postgres {
                    will_init: false,
                    rec: Bytes::from_static(b"\xbc\0\0\0\0\0\0\0h?m\x01\0\0\0\0p\n\0\09\x08\xa3\xea\0 \x8c\0\x7f\x06\0\0\xd22\0\0\xeb\x04\0\0\0\0\0\0\xff\x02\0@\0\0another_table\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x98\x08\0\0\x02@\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0@\0\0\0\0\0\0\x05\0\0\0\0@zD\x05\0\0\0\0\0\0\0\0\0pr\x01\0\0\0\0\0\0\0\0\x01d\0\0\0\0\0\0\x04\0\0\x01\0\0\0\x02\0")
                }
            )
        ]
    }

    struct RedoHarness {
        // underscored because unused, except for removal at drop
        _repo_dir: camino_tempfile::Utf8TempDir,
        manager: PostgresRedoManager,
    }

    impl RedoHarness {
        fn new() -> anyhow::Result<Self> {
            crate::tenant::harness::setup_logging();

            let repo_dir = camino_tempfile::tempdir()?;
            let conf = PageServerConf::dummy_conf(repo_dir.path().to_path_buf());
            let conf = Box::leak(Box::new(conf));
            let tenant_id = TenantId::generate();

            let manager = PostgresRedoManager::new(conf, tenant_id);

            Ok(RedoHarness {
                _repo_dir: repo_dir,
                manager,
            })
        }
    }
}
