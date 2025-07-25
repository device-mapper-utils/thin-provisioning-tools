use anyhow::{anyhow, Result};
use rand::prelude::*;
use std::collections::{BTreeMap, VecDeque};
use std::fs::OpenOptions;
use std::ops::Range;
use std::path::Path;
use thinp::thin::ir::{self, MetadataVisitor};
use thinp::thin::xml;

//------------------------------------------

pub trait XmlGen {
    fn generate_xml(&mut self, v: &mut dyn MetadataVisitor) -> Result<()>;
}

pub fn write_xml(path: &Path, g: &mut dyn XmlGen) -> Result<()> {
    let xml_out = OpenOptions::new()
        .read(false)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut w = xml::XmlWriter::new(xml_out);

    g.generate_xml(&mut w)
}

fn common_sb(nr_blocks: u64, time: u32) -> ir::Superblock {
    ir::Superblock {
        uuid: "".to_string(),
        time,
        transaction: 1,
        flags: None,
        version: None,
        data_block_size: 128,
        nr_data_blocks: nr_blocks,
        metadata_snap: None,
    }
}

//------------------------------------------

pub struct EmptyPoolS {
    pub old_nr_data_blocks: u64,
    pub new_nr_data_blocks: u64,
}

impl EmptyPoolS {
    pub fn new(old_nr_data_blocks: u64, new_nr_data_blocks: u64) -> Self {
        Self {
            old_nr_data_blocks,
            new_nr_data_blocks,
        }
    }
}

impl XmlGen for EmptyPoolS {
    fn generate_xml(&mut self, v: &mut dyn MetadataVisitor) -> Result<()> {
        v.superblock_b(&common_sb(self.old_nr_data_blocks, 0))?;
        v.superblock_e()?;
        Ok(())
    }
}

//------------------------------------------

pub struct SingleThinS {
    pub offset: u64,
    pub len: u64,
    pub old_nr_data_blocks: u64,
    pub new_nr_data_blocks: u64,
}

impl SingleThinS {
    pub fn new(offset: u64, len: u64, old_nr_data_blocks: u64, new_nr_data_blocks: u64) -> Self {
        SingleThinS {
            offset,
            len,
            old_nr_data_blocks,
            new_nr_data_blocks,
        }
    }
}

impl XmlGen for SingleThinS {
    fn generate_xml(&mut self, v: &mut dyn MetadataVisitor) -> Result<()> {
        v.superblock_b(&common_sb(self.old_nr_data_blocks, 0))?;
        v.device_b(&ir::Device {
            dev_id: 0,
            mapped_blocks: self.len,
            transaction: 0,
            creation_time: 0,
            snap_time: 0,
        })?;
        v.map(&ir::Map {
            thin_begin: 0,
            data_begin: self.offset,
            time: 0,
            len: self.len,
        })?;
        v.device_e()?;
        v.superblock_e()?;
        Ok(())
    }
}

//------------------------------------------

pub struct FragmentedS {
    pub nr_thins: u32,
    pub thin_size: u64,
    pub old_nr_data_blocks: u64,
    pub new_nr_data_blocks: u64,
}

impl FragmentedS {
    pub fn new(nr_thins: u32, thin_size: u64) -> Self {
        let old_size = (nr_thins as u64) * thin_size;
        FragmentedS {
            nr_thins,
            thin_size,
            old_nr_data_blocks: (nr_thins as u64) * thin_size,
            new_nr_data_blocks: old_size * 3 / 4,
        }
    }
}

#[derive(Clone)]
struct ThinRun {
    thin_id: u32,
    thin_begin: u64,
    len: u64,
}

#[derive(Clone, Debug, Copy)]
struct MappedRun {
    thin_id: u32,
    thin_begin: u64,
    data_begin: u64,
    len: u64,
}

fn mk_runs(thin_id: u32, total_len: u64, run_len: std::ops::Range<u64>) -> Vec<ThinRun> {
    let mut runs = Vec::new();
    let mut b = 0u64;
    while b < total_len {
        let len = u64::min(
            total_len - b,
            rand::rng().random_range(run_len.start..run_len.end),
        );
        runs.push(ThinRun {
            thin_id,
            thin_begin: b,
            len,
        });
        b += len;
    }
    runs
}

// the runs should be sorted by thin_id
fn count_mapped_blocks(runs: &[MappedRun]) -> Result<BTreeMap<u32, (u64, Range<usize>)>> {
    if runs.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut results = BTreeMap::new();
    let mut id = runs[0].thin_id;
    let mut len = runs[0].len;
    let mut range = Range::<usize> { start: 0, end: 1 };

    for m in runs.iter().skip(1) {
        if m.thin_id != id {
            results.insert(id, (len, range.clone()));
            id = m.thin_id;
            len = m.len;
            range.start = range.end;
            range.end += 1;
            continue;
        }

        len += m.len;
        range.end += 1;
    }

    results.insert(id, (len, range));
    Ok(results)
}

impl XmlGen for FragmentedS {
    fn generate_xml(&mut self, v: &mut dyn MetadataVisitor) -> Result<()> {
        // Allocate each thin fully, in runs between 1 and 16.
        let mut runs = Vec::new();
        for thin in 0..self.nr_thins {
            runs.append(&mut mk_runs(thin, self.thin_size, 1..17));
        }

        // Shuffle
        runs.shuffle(&mut rand::rng());

        // map across the data
        let mut maps = Vec::new();
        let mut b = 0;
        for r in &runs {
            maps.push(MappedRun {
                thin_id: r.thin_id,
                thin_begin: r.thin_begin,
                data_begin: b,
                len: r.len,
            });
            b += r.len;
        }

        // drop half the mappings, which leaves us free runs
        let mut dropped = Vec::new();
        for (i, m) in maps.iter().enumerate() {
            if i % 2 == 0 {
                dropped.push(*m);
            }
        }

        // Unshuffle.  This isn't strictly necc. but makes the xml
        // more readable.
        use std::cmp::Ordering;
        dropped.sort_by(|&l, &r| match l.thin_id.cmp(&r.thin_id) {
            Ordering::Equal => l.thin_begin.cmp(&r.thin_begin),
            o => o,
        });

        let stats = count_mapped_blocks(&dropped)?;

        // write the xml
        v.superblock_b(&common_sb(self.old_nr_data_blocks, 0))?;
        for thin in 0..self.nr_thins {
            let (mapped_blocks, range) = stats.get(&thin).cloned().unwrap_or((0, Range::default()));

            v.device_b(&ir::Device {
                dev_id: thin,
                mapped_blocks,
                transaction: 0,
                creation_time: 0,
                snap_time: 0,
            })?;

            for m in &dropped[range.start..range.end] {
                assert!(m.thin_id == thin);

                v.map(&ir::Map {
                    thin_begin: m.thin_begin,
                    data_begin: m.data_begin,
                    time: 0,
                    len: m.len,
                })?;
            }

            v.device_e()?;
        }
        v.superblock_e()?;
        Ok(())
    }
}

//------------------------------------------

struct Allocator {
    runs: VecDeque<Range<u64>>,
}

impl Allocator {
    fn new_shuffled(total_len: u64, run_len: Range<u64>) -> Allocator {
        let mut runs = Vec::new();

        let mut b = 0u64;
        while b < total_len {
            let len = u64::min(
                total_len - b,
                rand::rng().random_range(run_len.start..run_len.end),
            );
            runs.push(b..(b + len));
            b += len;
        }

        runs.shuffle(&mut rand::rng());
        let runs: VecDeque<Range<u64>> = runs.iter().cloned().collect();
        Allocator { runs }
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    fn alloc(&mut self, len: u64) -> Result<Vec<Range<u64>>> {
        let mut len = len;
        let mut runs = Vec::new();

        while len > 0 {
            let r = self.runs.pop_front();

            if r.is_none() {
                return Err(anyhow!("could not allocate; out of space"));
            }

            let r = r.unwrap();
            let rlen = r.end - r.start;
            if len < rlen {
                runs.push(r.start..(r.start + len));

                // We need to push something back.
                self.runs.push_front((r.start + len)..r.end);
                len = 0;
            } else {
                runs.push(r.start..r.end);
                len -= rlen;
            }
        }

        Ok(runs)
    }
}

// Having explicitly unmapped regions makes it easier to
// apply snapshots.
#[derive(Clone, Debug)]
enum Run {
    Mapped {
        data_begin: u64,
        time: u32,
        len: u64,
    },
    UnMapped {
        len: u64,
    },
}

impl Run {
    #[allow(dead_code)]
    fn len(&self) -> u64 {
        match self {
            Run::Mapped { len, .. } => *len,
            Run::UnMapped { len } => *len,
        }
    }

    fn mapped_len(&self) -> u64 {
        match self {
            Run::Mapped { len, .. } => *len,
            Run::UnMapped { .. } => 0,
        }
    }

    fn split(&self, n: u64) -> (Option<Run>, Option<Run>) {
        if n == 0 {
            (None, Some(self.clone()))
        } else if self.len() <= n {
            (Some(self.clone()), None)
        } else {
            match self {
                Run::Mapped {
                    data_begin,
                    time,
                    len,
                } => (
                    Some(Run::Mapped {
                        data_begin: *data_begin,
                        time: *time,
                        len: n,
                    }),
                    Some(Run::Mapped {
                        data_begin: data_begin + n,
                        time: *time,
                        len: len - n,
                    }),
                ),
                Run::UnMapped { len } => (
                    Some(Run::UnMapped { len: n }),
                    Some(Run::UnMapped { len: len - n }),
                ),
            }
        }
    }
}

#[derive(Clone)]
struct ThinDev {
    thin_id: u32,
    dev_size: u64,
    mapped_blocks: u64,
    runs: Vec<Run>,
    creation_time: u32,
    snap_time: u32,
}

impl ThinDev {
    fn emit(&self, v: &mut dyn MetadataVisitor) -> Result<()> {
        v.device_b(&ir::Device {
            dev_id: self.thin_id,
            mapped_blocks: self.mapped_blocks,
            transaction: 0,
            creation_time: self.creation_time,
            snap_time: self.snap_time,
        })?;

        let mut b = 0;
        for r in &self.runs {
            match r {
                Run::Mapped {
                    data_begin,
                    time,
                    len,
                } => {
                    v.map(&ir::Map {
                        thin_begin: b,
                        data_begin: *data_begin,
                        time: *time,
                        len: *len,
                    })?;
                    b += len;
                }
                Run::UnMapped { len } => {
                    b += len;
                }
            }
        }

        v.device_e()?;
        Ok(())
    }
}

// Denotes the type of io operation to a specific range of blocks.
// The operation could be noop (unchanged), overwrite, or discard.
#[derive(Clone, Debug)]
enum SnapRunType {
    Same,
    Diff,
    Hole,
}

#[derive(Clone, Debug)]
struct SnapRun(SnapRunType, u64);

fn mk_origin(
    thin_id: u32,
    total_len: u64,
    percent_mapped: usize,
    allocator: &mut Allocator,
    creation_time: u32,
    snap_time: u32,
) -> Result<ThinDev> {
    let mut runs = Vec::new();
    let mut total_mapped = 0;
    let mut b = 0;

    while b < total_len {
        let len = u64::min(rand::rng().random_range(16..64), total_len - b);

        let n = rand::rng().random_range(0..100);

        if n < percent_mapped {
            for data in allocator.alloc(len)? {
                assert!(data.end >= data.start);
                runs.push(Run::Mapped {
                    data_begin: data.start,
                    time: creation_time,
                    len: data.end - data.start,
                });
            }
            total_mapped += len;
        } else {
            runs.push(Run::UnMapped { len });
        }

        b += len;
    }

    Ok(ThinDev {
        thin_id,
        dev_size: total_len,
        mapped_blocks: total_mapped,
        runs,
        creation_time,
        snap_time,
    })
}

fn mk_snap_mapping(
    total_len: u64,
    run_len: Range<u64>,
    same_percent: usize,
    diff_percent: usize,
) -> Vec<SnapRun> {
    let mut runs = Vec::new();

    let mut b = 0u64;
    while b < total_len {
        let len = u64::min(
            total_len - b,
            rand::rng().random_range(run_len.start..run_len.end),
        );

        let n = rand::rng().random_range(0..100);

        if n < same_percent {
            runs.push(SnapRun(SnapRunType::Same, len));
        } else if n < diff_percent {
            runs.push(SnapRun(SnapRunType::Diff, len));
        } else {
            runs.push(SnapRun(SnapRunType::Hole, len));
        }

        b += len;
    }

    runs
}

fn mk_snapshot(
    thin_id: u32,
    origin: &ThinDev,
    percent_change: usize,
    allocator: &mut Allocator,
    creation_time: u32,
    snap_time: u32,
) -> Result<ThinDev> {
    // among the changed mappings, half are overwritten, and the other half are discarded
    let same_percent = 100 - percent_change;
    let diff_percent = same_percent + percent_change / 2;

    let snap_runs = mk_snap_mapping(origin.dev_size, 16..64, same_percent, diff_percent);
    let (runs, total_mapped) = apply_snap_runs(&origin.runs, &snap_runs, allocator, creation_time)?;

    Ok(ThinDev {
        thin_id,
        dev_size: origin.dev_size,
        mapped_blocks: total_mapped,
        runs,
        creation_time,
        snap_time,
    })
}

fn split_runs(mut n: u64, runs: &[Run]) -> (Vec<Run>, Vec<Run>) {
    let mut before = Vec::new();
    let mut after = Vec::new();

    for r in runs {
        match r.split(n) {
            (Some(lhs), None) => {
                before.push(lhs);
            }
            (Some(lhs), Some(rhs)) => {
                before.push(lhs);
                after.push(rhs);
            }
            (None, Some(rhs)) => {
                after.push(rhs);
            }
            (None, None) => {}
        }
        n = n.saturating_sub(r.len());
    }

    (before, after)
}

fn apply_snap_runs(
    origin: &[Run],
    snap: &[SnapRun],
    allocator: &mut Allocator,
    creation_time: u32,
) -> Result<(Vec<Run>, u64)> {
    let mut origin = origin.to_owned();
    let mut runs = Vec::new();
    let mut total_mapped = 0;

    for SnapRun(st, slen) in snap {
        let (os, rest) = split_runs(*slen, &origin);
        match st {
            SnapRunType::Same => {
                for o in os {
                    total_mapped += o.mapped_len();
                    runs.push(o);
                }
            }
            SnapRunType::Diff => {
                for data in allocator.alloc(*slen)? {
                    runs.push(Run::Mapped {
                        data_begin: data.start,
                        time: creation_time,
                        len: data.end - data.start,
                    });
                }
                total_mapped += *slen;
            }
            SnapRunType::Hole => {
                runs.push(Run::UnMapped { len: *slen });
            }
        }

        origin = rest;
    }

    Ok((runs, total_mapped))
}

// Snapshots share mappings, not necessarily the entire ranges.
pub struct SnapS {
    pub len: u64,
    pub nr_snaps: u32,

    // Snaps will differ from the origin by this percentage
    pub percent_change: usize,
    pub old_nr_data_blocks: u64,
    pub new_nr_data_blocks: u64,
}

impl SnapS {
    pub fn new(len: u64, nr_snaps: u32, percent_change: usize) -> Self {
        let delta = len * (nr_snaps as u64) * (percent_change as u64) / 100;
        let old_nr_data_blocks = len + 3 * delta;
        let new_nr_data_blocks = len + 2 * delta;

        SnapS {
            len,
            nr_snaps,
            percent_change,
            old_nr_data_blocks,
            new_nr_data_blocks,
        }
    }
}

impl XmlGen for SnapS {
    fn generate_xml(&mut self, v: &mut dyn MetadataVisitor) -> Result<()> {
        let mut allocator = Allocator::new_shuffled(self.old_nr_data_blocks, 64..512);
        let mut creation_time = 0;
        let mut snap_time = if self.nr_snaps > 1 { 1 } else { 0 };
        let mut origin = mk_origin(0, self.len, 50, &mut allocator, creation_time, snap_time)?;

        let time = self.nr_snaps - 1; // timestamp increases by 1 as a snapshot is created
        v.superblock_b(&common_sb(self.old_nr_data_blocks, time))?;
        origin.emit(v)?;

        // create snapshots recursively,
        // i.e., a new snapshot is derived from the latest created one.
        for thin in 1..self.nr_snaps {
            creation_time += 1;
            if thin != self.nr_snaps - 1 {
                snap_time += 1
            }
            let snap = mk_snapshot(
                thin,
                &origin,
                self.percent_change,
                &mut allocator,
                creation_time,
                snap_time,
            )?;
            snap.emit(v)?;
            origin = snap;
        }

        v.superblock_e()?;

        Ok(())
    }
}

//------------------------------------------
