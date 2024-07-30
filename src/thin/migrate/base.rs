use anyhow::{anyhow, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use crate::copier::batcher::*;
use crate::copier::sync_copier::*;
use crate::copier::wrapper::ThreadedCopier;
use crate::copier::*;
use crate::file_utils;
use crate::io_engine::utils::*;
use crate::io_engine::*;
use crate::report::*;
use crate::thin::metadata::*;
use crate::thin::migrate::devices::*;
use crate::thin::migrate::stream::*;

//------------------------------------------

const DEFAULT_BUFFER_SIZE: usize = 131_072; // 64 MiB in sectors

#[derive(Debug, PartialEq)]
pub struct SourceArgs {
    pub path: PathBuf,
    pub delta_id: Option<ThinId>,
}

pub struct FileDestArgs {
    pub path: PathBuf,
    pub create: bool,
}

pub enum DestArgs {
    Dev(PathBuf),
    File(FileDestArgs),
}

pub struct ThinMigrateOptions {
    pub source: SourceArgs,
    pub dest: DestArgs,
    pub zero_dest: bool,
    pub buffer_size: Option<usize>, // in sectors
    pub report: Arc<Report>,
}

fn mk_engine<P: AsRef<Path>>(path: P) -> Result<Arc<dyn IoEngine + Send + Sync>> {
    let engine = SyncIoEngine::new_with(path, false, false)?;
    Ok(Arc::new(engine))
}

pub fn metadata_dev_from_thin(scanner: &mut DmScanner, thin: &File) -> Result<DeviceNr> {
    let thin_name = scanner.file_to_name(thin)?.clone();
    let thin_table = get_thin_table(scanner, &thin_name)?;
    let pool_name = scanner.dev_to_name(&thin_table.pool_dev)?.clone();
    let pool_table = get_pool_table(scanner, &pool_name)?;
    Ok(pool_table.metadata_dev)
}

struct Source {
    file: File,
    stream: Box<dyn Stream>,
    block_size: usize, // in sectors
}

fn open_source(scanner: &mut DmScanner, src: &SourceArgs) -> Result<Source> {
    let thin = OpenOptions::new()
        .read(true)
        .write(false)
        .custom_flags(libc::O_EXCL | libc::O_DIRECT)
        .open(&src.path)?;
    let thin_name = scanner.file_to_name(&thin)?.clone();
    let thin_table = get_thin_table(scanner, &thin_name)?;
    let pool_name = scanner.dev_to_name(&thin_table.pool_dev)?.clone();
    let pool_table = get_pool_table(scanner, &pool_name)?;
    let metadata_dev = metadata_dev_from_thin(scanner, &thin)?;
    let metadata_path = scanner.dev_to_path(&metadata_dev)?.unwrap();
    let metadata_engine = mk_engine(metadata_path)?;

    let stream = Box::new(ThinStream::new(&metadata_engine, thin_table.thin_id)?);

    Ok(Source {
        file: thin,
        stream,
        block_size: pool_table.data_block_size as usize,
    })
}

struct Dest {
    file: File,
}

fn open_dest_dev(path: &PathBuf, expected_len: u64) -> Result<Dest> {
    let out = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL | libc::O_DIRECT)
        .open(path)?;
    let actual_len = file_utils::file_size(path)?;
    if actual_len != expected_len {
        return Err(anyhow!(
            "lengths differ: input({}) != output({})",
            expected_len,
            actual_len
        ));
    }
    Ok(Dest { file: out })
}

fn open_dest_file(path: &PathBuf, create: bool, expected_len: u64) -> Result<File> {
    if create {
        let out = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)?;
        out.set_len(expected_len)?;
        Ok(out)
    } else {
        let out = OpenOptions::new().read(true).write(true).open(path)?;
        let actual_len = file_utils::file_size(path)?;
        if actual_len != expected_len {
            return Err(anyhow!(
                "lengths differ input({}) != output({})",
                expected_len,
                actual_len
            ));
        }
        Ok(out)
    }
}

fn open_dest(_scanner: &mut DmScanner, dst: &DestArgs, expected_len: u64) -> Result<File> {
    match dst {
        DestArgs::Dev(path) => open_dest_dev(path, expected_len).map(|dst| dst.file),
        DestArgs::File(fdest) => open_dest_file(&fdest.path, fdest.create, expected_len),
    }
}

fn copy_regions(
    mut stream: Box<dyn Stream>,
    in_file: File,
    out_file: File,
    block_size: usize,
    buffer_size: usize,
    report: Arc<Report>,
) -> Result<()> {
    let in_vio: VectoredBlockIo<File> = in_file.into();
    let out_vio: VectoredBlockIo<File> = out_file.into();
    let copier = SyncCopier::new(
        buffer_size << SECTOR_SHIFT,
        block_size << SECTOR_SHIFT,
        in_vio,
        out_vio,
    )?;

    let (tx, rx) = mpsc::sync_channel::<Vec<CopyOp>>(1);
    let mut batcher = CopyOpBatcher::new(buffer_size / block_size, tx);

    let copier = ThreadedCopier::new(copier);
    let progress = Arc::new(ProgressReporter::new(
        report,
        stream.size_hint() / block_size as u64,
    ));
    let handle = copier.run(rx, progress);

    while let Some(chunk) = stream.next_chunk()? {
        match chunk.contents {
            ChunkContents::Skip => {
                // do nothing
            }
            ChunkContents::Copy => {
                let begin = chunk.offset / block_size as u64;
                let end = (chunk.offset + chunk.len) / block_size as u64;
                for b in begin..end {
                    batcher.push(CopyOp { src: b, dst: b })?;
                }
            }
            ChunkContents::Discard => {
                // Only needed when migrating a delta
                todo!();
            }
        }
    }

    batcher.complete()?;
    handle.join().unwrap()?;

    Ok(())
}

pub fn migrate(opts: ThinMigrateOptions) -> Result<()> {
    let mut scanner = DmScanner::new()?;
    let src = open_source(&mut scanner, &opts.source)?;
    let expected_len = file_utils::file_size(opts.source.path)?;
    let out_file = open_dest(&mut scanner, &opts.dest, expected_len)?;

    let buffer_size = opts
        .buffer_size
        .unwrap_or_else(|| std::cmp::max(src.block_size, DEFAULT_BUFFER_SIZE));

    copy_regions(
        src.stream,
        src.file,
        out_file,
        src.block_size,
        buffer_size,
        opts.report,
    )
}

//------------------------------------------
