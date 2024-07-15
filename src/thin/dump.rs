use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::checksum;
use crate::commands::engine::*;
use crate::dump_utils::*;
use crate::io_engine::*;
use crate::pdata::btree::*;
use crate::pdata::space_map::common::*;
use crate::pdata::unpack::*;
use crate::report::*;
use crate::thin::block_time::*;
use crate::thin::human_readable_format::HumanReadableWriter;
use crate::thin::ir::{self, MetadataVisitor};
use crate::thin::metadata::*;
use crate::thin::metadata_repair::*;
use crate::thin::superblock::*;
use crate::thin::xml;

//------------------------------------------

pub struct RunBuilder {
    run: Option<ir::Map>,
}

impl RunBuilder {
    pub fn new() -> RunBuilder {
        RunBuilder { run: None }
    }

    pub fn next(&mut self, thin_block: u64, data_block: u64, time: u32) -> Option<ir::Map> {
        use ir::Map;

        match self.run {
            None => {
                self.run = Some(ir::Map {
                    thin_begin: thin_block,
                    data_begin: data_block,
                    time,
                    len: 1,
                });
                None
            }
            Some(ir::Map {
                thin_begin,
                data_begin,
                time: mtime,
                len,
            }) => {
                if thin_block == (thin_begin + len)
                    && data_block == (data_begin + len)
                    && mtime == time
                {
                    self.run.as_mut().unwrap().len += 1;
                    None
                } else {
                    self.run.replace(Map {
                        thin_begin: thin_block,
                        data_begin: data_block,
                        time,
                        len: 1,
                    })
                }
            }
        }
    }

    pub fn complete(&mut self) -> Option<ir::Map> {
        self.run.take()
    }
}

impl Default for RunBuilder {
    fn default() -> Self {
        Self::new()
    }
}

//------------------------------------------

struct MVInner<'a> {
    md_out: &'a mut dyn MetadataVisitor,
    builder: RunBuilder,
}

struct MappingVisitor<'a> {
    inner: Mutex<MVInner<'a>>,
}

//------------------------------------------

impl<'a> MappingVisitor<'a> {
    fn new(md_out: &'a mut dyn MetadataVisitor) -> MappingVisitor<'a> {
        MappingVisitor {
            inner: Mutex::new(MVInner {
                md_out,
                builder: RunBuilder::new(),
            }),
        }
    }

    fn visit(
        &self,
        _path: &[u64],
        _kr: &KeyRange,
        _h: &NodeHeader,
        keys: &[u64],
        values: &[BlockTime],
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        for (k, v) in keys.iter().zip(values.iter()) {
            if let Some(run) = inner.builder.next(*k, v.block, v.time) {
                inner.md_out.map(&run)?;
            }
        }

        Ok(())
    }

    fn end_walk(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(run) = inner.builder.complete() {
            inner.md_out.map(&run)?;
        }
        Ok(())
    }
}

//------------------------------------------

struct OutputVisitor<'a> {
    out: &'a mut dyn MetadataVisitor,
}

impl<'a> OutputVisitor<'a> {
    fn new(out: &'a mut dyn MetadataVisitor) -> Self {
        Self { out }
    }
}

impl<'a> MetadataVisitor for OutputVisitor<'a> {
    fn superblock_b(&mut self, sb: &ir::Superblock) -> Result<ir::Visit> {
        output_context(self.out.superblock_b(sb))
    }

    fn superblock_e(&mut self) -> Result<ir::Visit> {
        output_context(self.out.superblock_e())
    }

    fn def_shared_b(&mut self, name: &str) -> Result<ir::Visit> {
        output_context(self.out.def_shared_b(name))
    }

    fn def_shared_e(&mut self) -> Result<ir::Visit> {
        output_context(self.out.def_shared_e())
    }

    fn device_b(&mut self, d: &ir::Device) -> Result<ir::Visit> {
        output_context(self.out.device_b(d))
    }

    fn device_e(&mut self) -> Result<ir::Visit> {
        output_context(self.out.device_e())
    }

    fn map(&mut self, m: &ir::Map) -> Result<ir::Visit> {
        output_context(self.out.map(m))
    }

    fn ref_shared(&mut self, name: &str) -> Result<ir::Visit> {
        output_context(self.out.ref_shared(name))
    }

    fn eof(&mut self) -> Result<ir::Visit> {
        output_context(self.out.eof())
    }
}

//------------------------------------------

#[derive(Clone)]
pub enum OutputFormat {
    XML,
    HumanReadable,
}

impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "xml" => Ok(OutputFormat::XML),
            "human_readable" => Ok(OutputFormat::HumanReadable),
            _ => Err(anyhow!("unknown format")),
        }
    }
}

pub struct ThinDumpOptions<'a> {
    pub input: &'a Path,
    pub output: Option<&'a Path>,
    pub engine_opts: EngineOptions,
    pub report: Arc<Report>,
    pub repair: bool,
    pub skip_mappings: bool,
    pub overrides: SuperblockOverrides,
    pub selected_devs: Option<Vec<u64>>,
    pub format: OutputFormat,
}

struct ThinDumpContext {
    report: Arc<Report>,
    engine: Arc<dyn IoEngine + Send + Sync>,
}

fn mk_context(opts: &ThinDumpOptions) -> Result<ThinDumpContext> {
    let engine = EngineBuilder::new(opts.input, &opts.engine_opts)
        .exclusive(!opts.engine_opts.use_metadata_snap)
        .build()?;

    Ok(ThinDumpContext {
        report: opts.report.clone(),
        engine,
    })
}

//------------------------------------------

fn emit_leaf(v: &MappingVisitor, b: &Block) -> Result<()> {
    use Node::*;
    let path = Vec::new();
    let kr = KeyRange::new();

    let bt = checksum::metadata_block_type(b.get_data());
    if bt != checksum::BT::NODE {
        return Err(anyhow!("checksum failed for node {}, {:?}", b.loc, bt));
    }

    let node = unpack_node::<BlockTime>(&path, b.get_data(), true, true)?;

    match node {
        Internal { .. } => Err(anyhow!("block {} is not a leaf", b.loc)),
        Leaf {
            header,
            keys,
            values,
        } => v.visit(&path, &kr, &header, &keys, &values),
    }
}

fn read_for<T>(engine: Arc<dyn IoEngine>, blocks: &[u64], mut t: T) -> Result<()>
where
    T: FnMut(Block) -> Result<()>,
{
    for cs in blocks.chunks(engine.get_batch_size()) {
        for b in engine
            .read_many(cs)
            .map_err(|_e| anyhow!("read_many failed"))?
        {
            let blk = b.map_err(|_e| anyhow!("read of individual block failed"))?;
            t(blk)?;
        }
    }

    Ok(())
}

fn emit_leaves(
    engine: Arc<dyn IoEngine>,
    out: &mut dyn MetadataVisitor,
    leaves: &[u64],
) -> Result<()> {
    let v = MappingVisitor::new(out);
    let proc = |b| {
        emit_leaf(&v, &b)?;
        Ok(())
    };

    read_for(engine, leaves, proc)?;
    v.end_walk()
}

fn emit_entries(
    engine: Arc<dyn IoEngine>,
    out: &mut dyn MetadataVisitor,
    entries: &[Entry],
) -> Result<()> {
    let mut leaves = Vec::new();

    for e in entries {
        match e {
            Entry::Leaf(b) => {
                leaves.push(*b);
            }
            Entry::Ref(id) => {
                if !leaves.is_empty() {
                    emit_leaves(engine.clone(), out, &leaves[0..])?;
                    leaves.clear();
                }
                let str = format!("{}", id);
                out.ref_shared(&str)?;
            }
        }
    }

    if !leaves.is_empty() {
        emit_leaves(engine, out, &leaves[0..])?;
    }

    Ok(())
}

fn to_superblock_ir(sb: &ThinSuperblock) -> Result<ir::Superblock> {
    match sb {
        ThinSuperblock::OnDisk(sb) => {
            let data_root = unpack::<SMRoot>(&sb.data_sm_root[0..])?;
            Ok(ir::Superblock {
                uuid: "".to_string(),
                time: sb.time,
                transaction: sb.transaction_id,
                flags: if sb.flags.needs_check { Some(1) } else { None },
                version: Some(sb.version),
                data_block_size: sb.data_block_size,
                nr_data_blocks: data_root.nr_blocks,
                metadata_snap: None,
            })
        }
        ThinSuperblock::InCore(sb) => Ok(ir::Superblock {
            uuid: "".to_string(),
            time: sb.time,
            transaction: sb.transaction_id,
            flags: if sb.flags.needs_check { Some(1) } else { None },
            version: Some(sb.version),
            data_block_size: sb.data_block_size,
            nr_data_blocks: sb.nr_data_blocks,
            metadata_snap: None,
        }),
    }
}

pub fn dump_metadata(
    engine: Arc<dyn IoEngine>,
    out: &mut dyn MetadataVisitor,
    sb: &ThinSuperblock,
    md: &Metadata,
) -> Result<()> {
    let out: &mut dyn MetadataVisitor = &mut OutputVisitor::new(out);

    let out_sb = to_superblock_ir(sb)?;
    out.superblock_b(&out_sb)?;

    for d in &md.defs {
        out.def_shared_b(&format!("{}", d.def_id))?;
        emit_entries(engine.clone(), out, &d.map.entries)?;
        out.def_shared_e()?;
    }

    for dev in &md.devs {
        let device = ir::Device {
            dev_id: dev.thin_id,
            mapped_blocks: dev.detail.mapped_blocks,
            transaction: dev.detail.transaction_id,
            creation_time: dev.detail.creation_time,
            snap_time: dev.detail.snapshotted_time,
        };
        out.device_b(&device)?;
        emit_entries(engine.clone(), out, &dev.map.entries)?;
        out.device_e()?;
    }
    out.superblock_e()?;
    out.eof()?;

    Ok(())
}

//------------------------------------------

pub fn dump_with_formatter(opts: ThinDumpOptions, out: &mut dyn MetadataVisitor) -> Result<()> {
    let ctx = mk_context(&opts)?;
    let sb = if opts.repair {
        read_or_rebuild_superblock(
            ctx.engine.clone(),
            ctx.report.clone(),
            SUPERBLOCK_LOCATION,
            &opts.overrides,
        )?
    } else if opts.engine_opts.use_metadata_snap {
        ThinSuperblock::OnDisk(read_superblock_snap(ctx.engine.as_ref())?)
    } else {
        ThinSuperblock::OnDisk(
            read_superblock(ctx.engine.as_ref(), SUPERBLOCK_LOCATION)
                .and_then(|sb| sb.overrides(&opts.overrides))?,
        )
    };

    let md = if opts.skip_mappings {
        build_metadata_without_mappings(ctx.engine.clone(), &sb)?
    } else {
        let m = build_metadata_with_dev(ctx.engine.clone(), &sb, opts.selected_devs)?;
        optimise_metadata(m)?
    };

    dump_metadata(ctx.engine, out, &sb, &md)
}

pub fn dump(opts: ThinDumpOptions) -> Result<()> {
    let writer: Box<dyn Write> = if opts.output.is_some() {
        let f = File::create(opts.output.unwrap()).context(OutputError)?;
        Box::new(BufWriter::new(f))
    } else {
        Box::new(BufWriter::new(std::io::stdout()))
    };

    let mut out: Box<dyn MetadataVisitor> = match opts.format {
        OutputFormat::XML => Box::new(xml::XmlWriter::new(writer)),
        OutputFormat::HumanReadable => Box::new(HumanReadableWriter::new(writer)),
    };

    dump_with_formatter(opts, out.as_mut())
}

//------------------------------------------
