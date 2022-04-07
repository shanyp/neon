//! An ImageLayer represents an image or a snapshot of a key-range at
//! one particular LSN. It contains an image of all key-value pairs
//! in its key-range. Any key that falls into the image layer's range
//! but does not exist in the layer, does not exist.
//!
//! An image layer is stored in a file on disk. The file is stored in
//! timelines/<timelineid> directory.  Currently, there are no
//! subdirectories, and each image layer file is named like this:
//!
//!    <key start>-<key end>__<LSN>
//!
//! For example:
//!
//!    000000067F000032BE0000400000000070B6-000000067F000032BE0000400000000080B6__00000000346BC568
//!
//! Every image layer file consists of three parts: "summary",
//! "index", and "values".  The summary is a fixed size header at the
//! beginning of the file, and it contains basic information about the
//! layer, and offsets to the other parts. The "index" is a serialized
//! HashMap, mapping from Key to an offset in the "values" part.  The
//! actual page images are stored in the "values" part.
//!
//! Only the "index" is loaded into memory by the load function.
//! When images are needed, they are read directly from disk.
//!
use crate::config::PageServerConf;
use crate::layered_repository::blob_io::{BlobCursor, BlobWriter, WriteBlobWriter};
use crate::layered_repository::block_io::{BlockReader, FileBlockReader};
use crate::layered_repository::filename::{ImageFileName, PathOrConf};
use crate::layered_repository::storage_layer::{
    BlobRef, Layer, ValueReconstructResult, ValueReconstructState,
};
use crate::page_cache::PAGE_SZ;
use crate::repository::{Key, Value};
use crate::virtual_file::VirtualFile;
use crate::{ZTenantId, ZTimelineId};
use crate::{IMAGE_FILE_MAGIC, STORAGE_FORMAT_VERSION};
use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use log::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::io::{Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, TryLockError};

use zenith_utils::bin_ser::BeSer;
use zenith_utils::lsn::Lsn;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Summary {
    /// Magic value to identify this as a zenith image file. Always IMAGE_FILE_MAGIC.
    magic: u16,
    format_version: u16,

    tenantid: ZTenantId,
    timelineid: ZTimelineId,
    key_range: Range<Key>,
    lsn: Lsn,

    /// Block number where the 'index' part of the file begins.
    index_start_blk: u32,
}

impl From<&ImageLayer> for Summary {
    fn from(layer: &ImageLayer) -> Self {
        Self {
            magic: IMAGE_FILE_MAGIC,
            format_version: STORAGE_FORMAT_VERSION,
            tenantid: layer.tenantid,
            timelineid: layer.timelineid,
            key_range: layer.key_range.clone(),

            lsn: layer.lsn,

            index_start_blk: 0,
        }
    }
}

///
/// ImageLayer is the in-memory data structure associated with an on-disk image
/// file.  We keep an ImageLayer in memory for each file, in the LayerMap. If a
/// layer is in "loaded" state, we have a copy of the index in memory, in 'inner'.
/// Otherwise the struct is just a placeholder for a file that exists on disk,
/// and it needs to be loaded before using it in queries.
///
pub struct ImageLayer {
    path_or_conf: PathOrConf,
    pub tenantid: ZTenantId,
    pub timelineid: ZTimelineId,
    pub key_range: Range<Key>,

    // This entry contains an image of all pages as of this LSN
    pub lsn: Lsn,

    inner: RwLock<ImageLayerInner>,
}

pub struct ImageLayerInner {
    /// If false, the 'index' has not been loaded into memory yet.
    loaded: bool,

    /// offset of each value
    index: HashMap<Key, BlobRef>,

    // values copied from summary
    index_start_blk: u32,

    /// Reader object for reading blocks from the file. (None if not loaded yet)
    file: Option<FileBlockReader<VirtualFile>>,
}

impl Layer for ImageLayer {
    fn filename(&self) -> PathBuf {
        PathBuf::from(self.layer_name().to_string())
    }

    fn get_tenant_id(&self) -> ZTenantId {
        self.tenantid
    }

    fn get_timeline_id(&self) -> ZTimelineId {
        self.timelineid
    }

    fn get_key_range(&self) -> Range<Key> {
        self.key_range.clone()
    }

    fn get_lsn_range(&self) -> Range<Lsn> {
        // End-bound is exclusive
        self.lsn..(self.lsn + 1)
    }

    /// Look up given page in the file
    fn get_value_reconstruct_data(
        &self,
        key: Key,
        lsn_range: Range<Lsn>,
        reconstruct_state: &mut ValueReconstructState,
    ) -> anyhow::Result<ValueReconstructResult> {
        assert!(self.key_range.contains(&key));
        assert!(lsn_range.end >= self.lsn);

        let inner = self.load()?;
        if let Some(blob_ref) = inner.index.get(&key) {
            let buf = inner
                .file
                .as_ref()
                .unwrap()
                .block_cursor()
                .read_blob(blob_ref.pos())
                .with_context(|| {
                    format!(
                        "failed to read blob from data file {} at offset {}",
                        self.filename().display(),
                        blob_ref.pos()
                    )
                })?;
            let value = Bytes::from(buf);

            reconstruct_state.img = Some((self.lsn, value));
            Ok(ValueReconstructResult::Complete)
        } else {
            Ok(ValueReconstructResult::Missing)
        }
    }

    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Key, Lsn, Value)>>> {
        todo!();
    }

    fn unload(&self) -> Result<()> {
        // Unload the index.
        //
        // TODO: we should access the index directly from pages on the disk,
        // using the buffer cache. This load/unload mechanism is really ad hoc.

        // FIXME: In debug mode, loading and unloading the index slows
        // things down so much that you get timeout errors. At least
        // with the test_parallel_copy test. So as an even more ad hoc
        // stopgap fix for that, only unload every on average 10
        // checkpoint cycles.
        use rand::RngCore;
        if rand::thread_rng().next_u32() > (u32::MAX / 10) {
            return Ok(());
        }

        let mut inner = match self.inner.try_write() {
            Ok(inner) => inner,
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Poisoned(_)) => panic!("ImageLayer lock was poisoned"),
        };
        inner.index = HashMap::default();
        inner.loaded = false;

        Ok(())
    }

    fn delete(&self) -> Result<()> {
        // delete underlying file
        fs::remove_file(self.path())?;
        Ok(())
    }

    fn is_incremental(&self) -> bool {
        false
    }

    fn is_in_memory(&self) -> bool {
        false
    }

    /// debugging function to print out the contents of the layer
    fn dump(&self, verbose: bool) -> Result<()> {
        println!(
            "----- image layer for ten {} tli {} key {}-{} at {} ----",
            self.tenantid, self.timelineid, self.key_range.start, self.key_range.end, self.lsn
        );

        if !verbose {
            return Ok(());
        }

        let inner = self.load()?;

        let mut index_vec: Vec<(&Key, &BlobRef)> = inner.index.iter().collect();
        index_vec.sort_by_key(|x| x.1.pos());

        for (key, blob_ref) in index_vec {
            println!("key: {} offset {}", key, blob_ref.pos());
        }

        Ok(())
    }
}

impl ImageLayer {
    fn path_for(
        path_or_conf: &PathOrConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        fname: &ImageFileName,
    ) -> PathBuf {
        match path_or_conf {
            PathOrConf::Path(path) => path.to_path_buf(),
            PathOrConf::Conf(conf) => conf
                .timeline_path(&timelineid, &tenantid)
                .join(fname.to_string()),
        }
    }

    ///
    /// Open the underlying file and read the metadata into memory, if it's
    /// not loaded already.
    ///
    fn load(&self) -> Result<RwLockReadGuard<ImageLayerInner>> {
        loop {
            // Quick exit if already loaded
            let inner = self.inner.read().unwrap();
            if inner.loaded {
                return Ok(inner);
            }

            // Need to open the file and load the metadata. Upgrade our lock to
            // a write lock. (Or rather, release and re-lock in write mode.)
            drop(inner);
            let mut inner = self.inner.write().unwrap();
            if !inner.loaded {
                self.load_inner(&mut inner)?;
            } else {
                // Another thread loaded it while we were not holding the lock.
            }

            // We now have the file open and loaded. There's no function to do
            // that in the std library RwLock, so we have to release and re-lock
            // in read mode. (To be precise, the lock guard was moved in the
            // above call to `load_inner`, so it's already been released). And
            // while we do that, another thread could unload again, so we have
            // to re-check and retry if that happens.
            drop(inner);
        }
    }

    fn load_inner(&self, inner: &mut ImageLayerInner) -> Result<()> {
        let path = self.path();

        // Open the file if it's not open already.
        if inner.file.is_none() {
            let file = VirtualFile::open(&path)
                .with_context(|| format!("Failed to open file '{}'", path.display()))?;
            inner.file = Some(FileBlockReader::new(file));
        }
        let file = inner.file.as_mut().unwrap();
        let summary_blk = file.read_blk(0)?;
        let actual_summary = Summary::des_prefix(summary_blk.as_ref())?;

        match &self.path_or_conf {
            PathOrConf::Conf(_) => {
                let mut expected_summary = Summary::from(self);
                expected_summary.index_start_blk = actual_summary.index_start_blk;

                if actual_summary != expected_summary {
                    bail!("in-file summary does not match expected summary. actual = {:?} expected = {:?}", actual_summary, expected_summary);
                }
            }
            PathOrConf::Path(path) => {
                let actual_filename = Path::new(path.file_name().unwrap());
                let expected_filename = self.filename();

                if actual_filename != expected_filename {
                    println!(
                        "warning: filename does not match what is expected from in-file summary"
                    );
                    println!("actual: {:?}", actual_filename);
                    println!("expected: {:?}", expected_filename);
                }
            }
        }

        file.file.seek(SeekFrom::Start(
            actual_summary.index_start_blk as u64 * PAGE_SZ as u64,
        ))?;
        let mut buf_reader = std::io::BufReader::new(&mut file.file);
        let index = HashMap::des_from(&mut buf_reader)?;

        inner.index_start_blk = actual_summary.index_start_blk;

        info!("loaded from {}", &path.display());

        inner.index = index;
        inner.loaded = true;
        Ok(())
    }

    /// Create an ImageLayer struct representing an existing file on disk
    pub fn new(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        filename: &ImageFileName,
    ) -> ImageLayer {
        ImageLayer {
            path_or_conf: PathOrConf::Conf(conf),
            timelineid,
            tenantid,
            key_range: filename.key_range.clone(),
            lsn: filename.lsn,
            inner: RwLock::new(ImageLayerInner {
                index: HashMap::new(),
                loaded: false,
                file: None,
                index_start_blk: 0,
            }),
        }
    }

    /// Create an ImageLayer struct representing an existing file on disk.
    ///
    /// This variant is only used for debugging purposes, by the 'dump_layerfile' binary.
    pub fn new_for_path<F>(path: &Path, file: F) -> Result<ImageLayer>
    where
        F: std::os::unix::prelude::FileExt,
    {
        let mut summary_buf = Vec::new();
        summary_buf.resize(PAGE_SZ, 0);
        file.read_exact_at(&mut summary_buf, 0)?;
        let summary = Summary::des_prefix(&summary_buf)?;

        Ok(ImageLayer {
            path_or_conf: PathOrConf::Path(path.to_path_buf()),
            timelineid: summary.timelineid,
            tenantid: summary.tenantid,
            key_range: summary.key_range,
            lsn: summary.lsn,
            inner: RwLock::new(ImageLayerInner {
                file: None,
                index: HashMap::new(),
                loaded: false,
                index_start_blk: 0,
            }),
        })
    }

    fn layer_name(&self) -> ImageFileName {
        ImageFileName {
            key_range: self.key_range.clone(),
            lsn: self.lsn,
        }
    }

    /// Path to the layer file in pageserver workdir.
    pub fn path(&self) -> PathBuf {
        Self::path_for(
            &self.path_or_conf,
            self.timelineid,
            self.tenantid,
            &self.layer_name(),
        )
    }
}

/// A builder object for constructing a new image layer.
///
/// Usage:
///
/// 1. Create the ImageLayerWriter by calling ImageLayerWriter::new(...)
///
/// 2. Write the contents by calling `put_page_image` for every key-value
///    pair in the key range.
///
/// 3. Call `finish`.
///
pub struct ImageLayerWriter {
    conf: &'static PageServerConf,
    _path: PathBuf,
    timelineid: ZTimelineId,
    tenantid: ZTenantId,
    key_range: Range<Key>,
    lsn: Lsn,

    index: HashMap<Key, BlobRef>,

    blob_writer: WriteBlobWriter<VirtualFile>,
}

impl ImageLayerWriter {
    pub fn new(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        key_range: &Range<Key>,
        lsn: Lsn,
    ) -> anyhow::Result<ImageLayerWriter> {
        // Create the file
        //
        // Note: This overwrites any existing file. There shouldn't be any.
        // FIXME: throw an error instead?
        let path = ImageLayer::path_for(
            &PathOrConf::Conf(conf),
            timelineid,
            tenantid,
            &ImageFileName {
                key_range: key_range.clone(),
                lsn,
            },
        );
        info!("new image layer {}", path.display());
        let file = VirtualFile::create(&path)?;
        let blob_writer = WriteBlobWriter::new(file, PAGE_SZ as u64);

        let writer = ImageLayerWriter {
            conf,
            _path: path,
            timelineid,
            tenantid,
            key_range: key_range.clone(),
            lsn,
            index: HashMap::new(),
            blob_writer,
        };

        Ok(writer)
    }

    ///
    /// Write next value to the file.
    ///
    /// The page versions must be appended in blknum order.
    ///
    pub fn put_image(&mut self, key: Key, img: &[u8]) -> Result<()> {
        ensure!(self.key_range.contains(&key));
        let off = self.blob_writer.write_blob(img)?;

        let old = self.index.insert(key, BlobRef::new(off, true));
        assert!(old.is_none());

        Ok(())
    }

    pub fn finish(self) -> anyhow::Result<ImageLayer> {
        let index_start_blk =
            ((self.blob_writer.size() + PAGE_SZ as u64 - 1) / PAGE_SZ as u64) as u32;

        let mut file = self.blob_writer.into_inner();

        // Write out the index
        let buf = HashMap::ser(&self.index)?;
        file.seek(SeekFrom::Start(index_start_blk as u64 * PAGE_SZ as u64))?;
        file.write_all(&buf)?;

        // Fill in the summary on blk 0
        let summary = Summary {
            magic: IMAGE_FILE_MAGIC,
            format_version: STORAGE_FORMAT_VERSION,
            tenantid: self.tenantid,
            timelineid: self.timelineid,
            key_range: self.key_range.clone(),
            lsn: self.lsn,
            index_start_blk,
        };
        file.seek(SeekFrom::Start(0))?;
        Summary::ser_into(&summary, &mut file)?;

        // Note: Because we open the file in write-only mode, we cannot
        // reuse the same VirtualFile for reading later. That's why we don't
        // set inner.file here. The first read will have to re-open it.
        let layer = ImageLayer {
            path_or_conf: PathOrConf::Conf(self.conf),
            timelineid: self.timelineid,
            tenantid: self.tenantid,
            key_range: self.key_range.clone(),
            lsn: self.lsn,
            inner: RwLock::new(ImageLayerInner {
                loaded: false,
                index: HashMap::new(),
                file: None,
                index_start_blk,
            }),
        };
        trace!("created image layer {}", layer.path().display());

        Ok(layer)
    }
}
