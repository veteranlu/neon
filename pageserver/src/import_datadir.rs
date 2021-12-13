//!
//! Import data and WAL from a PostgreSQL data directory and WAL segments into
//! a zenith Timeline.
//!
use std::fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use tracing::*;

use crate::relish::*;
use crate::repository::*;
use crate::walrecord::*;
use crate::walingest;
use postgres_ffi::relfile_utils::*;
use postgres_ffi::waldecoder::*;
use postgres_ffi::xlog_utils::*;
use postgres_ffi::Oid;
use postgres_ffi::{pg_constants, CheckPoint, ControlFileData};
use zenith_utils::lsn::Lsn;

///
/// Import all relation data pages from local disk into the repository.
///
pub fn import_timeline_from_postgres_datadir(
    path: &Path,
    writer: &dyn TimelineWriter,
    lsn: Lsn,
) -> Result<()> {
    let mut pg_control: Option<ControlFileData> = None;

    // Scan 'global'
    for direntry in fs::read_dir(path.join("global"))? {
        let direntry = direntry?;
        match direntry.file_name().to_str() {
            None => continue,

            Some("pg_control") => {
                pg_control = Some(import_control_file(writer, lsn, &direntry.path())?);
            }
            Some("pg_filenode.map") => import_nonrel_file(
                writer,
                lsn,
                RelishTag::FileNodeMap {
                    spcnode: pg_constants::GLOBALTABLESPACE_OID,
                    dbnode: 0,
                },
                &direntry.path(),
            )?,

            // Load any relation files into the page server
            _ => import_relfile(
                &direntry.path(),
                writer,
                lsn,
                pg_constants::GLOBALTABLESPACE_OID,
                0,
            )?,
        }
    }

    // Scan 'base'. It contains database dirs, the database OID is the filename.
    // E.g. 'base/12345', where 12345 is the database OID.
    for direntry in fs::read_dir(path.join("base"))? {
        let direntry = direntry?;

        //skip all temporary files
        if direntry.file_name().to_str().unwrap() == "pgsql_tmp" {
            continue;
        }

        let dboid = direntry.file_name().to_str().unwrap().parse::<u32>()?;

        for direntry in fs::read_dir(direntry.path())? {
            let direntry = direntry?;
            match direntry.file_name().to_str() {
                None => continue,

                Some("PG_VERSION") => continue,
                Some("pg_filenode.map") => import_nonrel_file(
                    writer,
                    lsn,
                    RelishTag::FileNodeMap {
                        spcnode: pg_constants::DEFAULTTABLESPACE_OID,
                        dbnode: dboid,
                    },
                    &direntry.path(),
                )?,

                // Load any relation files into the page server
                _ => import_relfile(
                    &direntry.path(),
                    writer,
                    lsn,
                    pg_constants::DEFAULTTABLESPACE_OID,
                    dboid,
                )?,
            }
        }
    }
    for entry in fs::read_dir(path.join("pg_xact"))? {
        let entry = entry?;
        import_slru_file(writer, lsn, SlruKind::Clog, &entry.path())?;
    }
    for entry in fs::read_dir(path.join("pg_multixact").join("members"))? {
        let entry = entry?;
        import_slru_file(writer, lsn, SlruKind::MultiXactMembers, &entry.path())?;
    }
    for entry in fs::read_dir(path.join("pg_multixact").join("offsets"))? {
        let entry = entry?;
        import_slru_file(writer, lsn, SlruKind::MultiXactOffsets, &entry.path())?;
    }
    for entry in fs::read_dir(path.join("pg_twophase"))? {
        let entry = entry?;
        let xid = u32::from_str_radix(entry.path().to_str().unwrap(), 16)?;
        import_nonrel_file(writer, lsn, RelishTag::TwoPhase { xid }, &entry.path())?;
    }
    // TODO: Scan pg_tblspc

    writer.advance_last_record_lsn(lsn);

    // Import WAL. This is needed even when starting from a shutdown checkpoint, because
    // this reads the checkpoint record itself, advancing the tip of the timeline to
    // *after* the checkpoint record. And crucially, it initializes the 'prev_lsn'
    let pg_control = pg_control.ok_or_else(|| anyhow!("pg_control file not found"))?;
    import_wal(
        &path.join("pg_wal"),
        writer,
        Lsn(pg_control.checkPointCopy.redo),
        lsn,
        &mut pg_control.checkPointCopy.clone(),
    )?;

    Ok(())
}

// subroutine of import_timeline_from_postgres_datadir(), to load one relation file.
fn import_relfile(
    path: &Path,
    timeline: &dyn TimelineWriter,
    lsn: Lsn,
    spcoid: Oid,
    dboid: Oid,
) -> Result<()> {
    // Does it look like a relation file?
    trace!("importing rel file {}", path.display());

    let p = parse_relfilename(path.file_name().unwrap().to_str().unwrap());
    if let Err(e) = p {
        warn!("unrecognized file in postgres datadir: {:?} ({})", path, e);
        return Err(e.into());
    }
    let (relnode, forknum, segno) = p.unwrap();

    let mut file = File::open(path)?;
    let mut buf: [u8; 8192] = [0u8; 8192];

    let mut blknum: u32 = segno * (1024 * 1024 * 1024 / pg_constants::BLCKSZ as u32);
    loop {
        let r = file.read_exact(&mut buf);
        match r {
            Ok(_) => {
                let rel = RelTag {
                    spcnode: spcoid,
                    dbnode: dboid,
                    relnode,
                    forknum,
                };
                let tag = RelishTag::Relation(rel);
                timeline.put_page_image(tag, blknum, lsn, Bytes::copy_from_slice(&buf))?;
            }

            // TODO: UnexpectedEof is expected
            Err(err) => match err.kind() {
                std::io::ErrorKind::UnexpectedEof => {
                    // reached EOF. That's expected.
                    // FIXME: maybe check that we read the full length of the file?
                    break;
                }
                _ => {
                    bail!("error reading file {}: {:#}", path.display(), err);
                }
            },
        };
        blknum += 1;
    }

    Ok(())
}

///
/// Import a "non-blocky" file into the repository
///
/// This is used for small files like the control file, twophase files etc. that
/// are just slurped into the repository as one blob.
///
fn import_nonrel_file(
    timeline: &dyn TimelineWriter,
    lsn: Lsn,
    tag: RelishTag,
    path: &Path,
) -> Result<()> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    // read the whole file
    file.read_to_end(&mut buffer)?;

    trace!("importing non-rel file {}", path.display());

    timeline.put_page_image(tag, 0, lsn, Bytes::copy_from_slice(&buffer[..]))?;
    Ok(())
}

///
/// Import pg_control file into the repository.
///
/// The control file is imported as is, but we also extract the checkpoint record
/// from it and store it separated.
fn import_control_file(
    timeline: &dyn TimelineWriter,
    lsn: Lsn,
    path: &Path,
) -> Result<ControlFileData> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    // read the whole file
    file.read_to_end(&mut buffer)?;

    trace!("importing control file {}", path.display());

    // Import it as ControlFile
    timeline.put_page_image(
        RelishTag::ControlFile,
        0,
        lsn,
        Bytes::copy_from_slice(&buffer[..]),
    )?;

    // Extract the checkpoint record and import it separately.
    let pg_control = ControlFileData::decode(&buffer)?;
    let checkpoint_bytes = pg_control.checkPointCopy.encode();
    timeline.put_page_image(RelishTag::Checkpoint, 0, lsn, checkpoint_bytes)?;

    Ok(pg_control)
}

///
/// Import an SLRU segment file
///
fn import_slru_file(
    timeline: &dyn TimelineWriter,
    lsn: Lsn,
    slru: SlruKind,
    path: &Path,
) -> Result<()> {
    // Does it look like an SLRU file?
    let mut file = File::open(path)?;
    let mut buf: [u8; 8192] = [0u8; 8192];
    let segno = u32::from_str_radix(path.file_name().unwrap().to_str().unwrap(), 16)?;

    trace!("importing slru file {}", path.display());

    let mut rpageno = 0;
    loop {
        let r = file.read_exact(&mut buf);
        match r {
            Ok(_) => {
                timeline.put_page_image(
                    RelishTag::Slru { slru, segno },
                    rpageno,
                    lsn,
                    Bytes::copy_from_slice(&buf),
                )?;
            }

            // TODO: UnexpectedEof is expected
            Err(err) => match err.kind() {
                std::io::ErrorKind::UnexpectedEof => {
                    // reached EOF. That's expected.
                    // FIXME: maybe check that we read the full length of the file?
                    break;
                }
                _ => {
                    bail!("error reading file {}: {:#}", path.display(), err);
                }
            },
        };
        rpageno += 1;

        // TODO: Check that the file isn't unexpectedly large, not larger than SLRU_PAGES_PER_SEGMENT pages
    }

    Ok(())
}

/// Scan PostgreSQL WAL files in given directory and load all records between
/// 'startpoint' and 'endpoint' into the repository.
fn import_wal(
    walpath: &Path,
    timeline: &dyn TimelineWriter,
    startpoint: Lsn,
    endpoint: Lsn,
    checkpoint: &mut CheckPoint,
) -> Result<()> {
    let mut waldecoder = WalStreamDecoder::new(startpoint);

    let mut segno = startpoint.segment_number(pg_constants::WAL_SEGMENT_SIZE);
    let mut offset = startpoint.segment_offset(pg_constants::WAL_SEGMENT_SIZE);
    let mut last_lsn = startpoint;

    while last_lsn <= endpoint {
        // FIXME: assume postgresql tli 1 for now
        let filename = XLogFileName(1, segno, pg_constants::WAL_SEGMENT_SIZE);
        let mut buf = Vec::new();

        // Read local file
        let mut path = walpath.join(&filename);

        // It could be as .partial
        if !PathBuf::from(&path).exists() {
            path = walpath.join(filename + ".partial");
        }

        // Slurp the WAL file
        let mut file = File::open(&path)?;

        if offset > 0 {
            file.seek(SeekFrom::Start(offset as u64))?;
        }

        let nread = file.read_to_end(&mut buf)?;
        if nread != pg_constants::WAL_SEGMENT_SIZE - offset as usize {
            // Maybe allow this for .partial files?
            error!("read only {} bytes from WAL file", nread);
        }

        waldecoder.feed_bytes(&buf);

        let mut nrecords = 0;
        while last_lsn <= endpoint {
            if let Some((lsn, recdata)) = waldecoder.poll_decode()? {
                let mut checkpoint_modified = false;

                let decoded = decode_wal_record(recdata.clone());
                walingest::save_decoded_record(
                    checkpoint,
                    &mut checkpoint_modified,
                    timeline,
                    &decoded,
                    recdata,
                    lsn,
                )?;
                last_lsn = lsn;

                if checkpoint_modified {
                    let checkpoint_bytes = checkpoint.encode();
                    timeline.put_page_image(
                        RelishTag::Checkpoint,
                        0,
                        last_lsn,
                        checkpoint_bytes,
                    )?;
                }

                // Now that this record has been fully handled, including updating the
                // checkpoint data, let the repository know that it is up-to-date to this LSN
                timeline.advance_last_record_lsn(last_lsn);
                nrecords += 1;

                trace!("imported record at {} (end {})", lsn, endpoint);
            }
        }

        debug!("imported {} records up to {}", nrecords, last_lsn);

        segno += 1;
        offset = 0;
    }

    if last_lsn != startpoint {
        debug!(
            "reached end of WAL at {}, updating checkpoint info",
            last_lsn
        );

        timeline.advance_last_record_lsn(last_lsn);
    } else {
        info!("no WAL to import at {}", last_lsn);
    }

    Ok(())
}