//! `recordings` subcommand: find and print the titles and metadata of every
//! recording on a TiVo drive.
//!
//! A recording's metadata lives across several linked MFS database objects:
//!
//! ```text
//! Recording (obj_type 14)
//!   └─ Showing (obj_type 7, stored inline as a subobject)
//!        ├─ Program (obj_type 3, referenced by fsid)  → Title, EpisodeTitle, ...
//!        └─ Station (obj_type 5, referenced by fsid)  → CallSign, Name
//! ```
//!
//! Field ids (the `attr` byte on each attribute) come from `schema.txt`.

use std::collections::HashMap;

use chrono::{TimeZone, Utc};
use ovit::TivoDrive;
use prettytable::{row, Table};
use tivo_media_file_system::{obj_type, MFSINodeType, MFSObject};

/// Database objects are small; anything larger is almost certainly not a tyDB
/// object and we should not pull it into memory while scanning.
const MAX_OBJECT_SIZE: u32 = 1 << 20;

// Program (obj_type 3) field ids.
const PROGRAM_TITLE: u8 = 17;
const PROGRAM_DESCRIPTION: u8 = 19;
const PROGRAM_MOVIE_YEAR: u8 = 22;
const PROGRAM_EPISODE_TITLE: u8 = 29;
const PROGRAM_ORIGINAL_AIR_DATE: u8 = 49;
const PROGRAM_SHORT_DESCRIPTION: u8 = 53;

// Station (obj_type 5) field ids.
const STATION_NAME: u8 = 17;
const STATION_CALL_SIGN: u8 = 18;

// Showing (obj_type 7) field ids.
const SHOWING_PROGRAM: u8 = 16;
const SHOWING_STATION: u8 = 17;
const SHOWING_DATE: u8 = 18;
const SHOWING_TIME: u8 = 19;

// Recording (obj_type 14) field ids.
const RECORDING_STREAM_FILE_SIZE: u8 = 54;

#[derive(Default, Clone)]
struct ProgramInfo {
    title: Option<String>,
    episode_title: Option<String>,
    description: Option<String>,
    original_air_date: Option<u32>,
    movie_year: Option<u32>,
}

#[derive(Default, Clone)]
struct StationInfo {
    call_sign: Option<String>,
    name: Option<String>,
}

struct Recording {
    fsid: u32,
    program_fsid: Option<u32>,
    station_fsid: Option<u32>,
    date: Option<u32>,
    time: Option<u32>,
    stream_size: Option<u32>,
}

fn program_info(object: &MFSObject) -> Option<ProgramInfo> {
    let program = object.first_of_type(obj_type::PROGRAM)?;
    Some(ProgramInfo {
        title: program.string(PROGRAM_TITLE),
        episode_title: program.string(PROGRAM_EPISODE_TITLE),
        description: program
            .string(PROGRAM_DESCRIPTION)
            .or_else(|| program.string(PROGRAM_SHORT_DESCRIPTION)),
        original_air_date: program.int(PROGRAM_ORIGINAL_AIR_DATE),
        movie_year: program.int(PROGRAM_MOVIE_YEAR),
    })
}

fn station_info(object: &MFSObject) -> Option<StationInfo> {
    let station = object.first_of_type(obj_type::STATION)?;
    Some(StationInfo {
        call_sign: station.string(STATION_CALL_SIGN),
        name: station.string(STATION_NAME),
    })
}

/// Build a `Recording` from an object that contains a Recording subobject. The
/// Showing is stored inline, so its Program/Station references and air time come
/// from the same buffer.
fn recording_from_object(fsid: u32, object: &MFSObject) -> Option<Recording> {
    let recording = object.first_of_type(obj_type::RECORDING)?;
    let showing = object.first_of_type(obj_type::SHOWING);

    Some(Recording {
        fsid,
        program_fsid: showing.and_then(|s| s.object_fsid(SHOWING_PROGRAM)),
        station_fsid: showing.and_then(|s| s.object_fsid(SHOWING_STATION)),
        date: showing.and_then(|s| s.int(SHOWING_DATE)),
        time: showing.and_then(|s| s.int(SHOWING_TIME)),
        stream_size: recording.int(RECORDING_STREAM_FILE_SIZE),
    })
}

fn read_object(drive: &mut TivoDrive, fsid: u32, input_path: &str) -> Option<MFSObject> {
    let inode = drive.get_inode_from_fsid(fsid).ok()?;
    // `get_inode_from_fsid` can fall back to returning a hashed inode whose fsid
    // doesn't match; ignore those so we never attribute the wrong object.
    if inode.fsid != fsid {
        return None;
    }
    let data = inode.get_data(input_path.to_string()).ok()?;
    Some(MFSObject::parse(&data))
}

fn tivo_date_time(date: Option<u32>, time: Option<u32>) -> String {
    match date {
        Some(date) if date > 0 => {
            // TiVo stores Date as days since the Unix epoch and Time as seconds
            // past midnight.
            let seconds = i64::from(date) * 86_400 + i64::from(time.unwrap_or(0));
            Utc.timestamp_opt(seconds, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| format!("day {}", date))
        }
        _ => String::new(),
    }
}

fn tivo_date(date: Option<u32>) -> String {
    match date {
        Some(date) if date > 0 => Utc
            .timestamp_opt(i64::from(date) * 86_400, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        text.to_string()
    }
}

/// Scan every inode, collecting recordings plus the Program and Station objects
/// they reference. This does not rely on the `/Recording` index directories, so
/// it still works on a drive whose directory structure is damaged or partial.
fn scan_all_inodes(
    drive: &mut TivoDrive,
    input_path: &str,
) -> (
    Vec<Recording>,
    HashMap<u32, ProgramInfo>,
    HashMap<u32, StationInfo>,
) {
    let mut recordings = Vec::new();
    let mut programs: HashMap<u32, ProgramInfo> = HashMap::new();
    let mut stations: HashMap<u32, StationInfo> = HashMap::new();

    let inodes = match drive.raw_zonemap.inode_iter() {
        Ok(iter) => iter,
        Err(err) => {
            eprintln!("Could not iterate inodes: {}", err);
            return (recordings, programs, stations);
        }
    };

    let mut scanned: u64 = 0;
    let mut db_inodes_read: u64 = 0;
    let mut objects_with_subobjects: u64 = 0;
    let mut type_histogram: HashMap<u16, u64> = HashMap::new();
    for inode in inodes {
        scanned += 1;
        if scanned % 50_000 == 0 {
            println!(
                "  ...scanned {} inodes ({} db objects read, {} parsed, {} recordings so far)",
                scanned,
                db_inodes_read,
                objects_with_subobjects,
                recordings.len()
            );
        }

        // Only database objects hold tyDB metadata. Skip everything else —
        // critically the media Stream inodes, whose data is the multi-gigabyte
        // MPEG recording and would otherwise be read entirely into memory.
        if inode.fsid == 0
            || inode.r#type != MFSINodeType::Db
            || inode.size == 0
            || inode.size > MAX_OBJECT_SIZE
        {
            continue;
        }

        let data = match inode.get_data(input_path.to_string()) {
            Ok(data) => data,
            Err(_) => continue,
        };
        if data.len() < 8 {
            continue;
        }
        db_inodes_read += 1;

        let object = MFSObject::parse(&data);
        if !object.subobjects.is_empty() {
            objects_with_subobjects += 1;
        }
        for subobject in &object.subobjects {
            *type_histogram.entry(subobject.obj_type).or_insert(0) += 1;
        }

        if let Some(program) = program_info(&object) {
            programs.insert(inode.fsid, program);
        }
        if let Some(station) = station_info(&object) {
            stations.insert(inode.fsid, station);
        }
        if let Some(recording) = recording_from_object(inode.fsid, &object) {
            recordings.push(recording);
        }
    }

    println!(
        "Scan complete: {} inodes, {} Db objects read, {} parsed into subobjects.",
        scanned, db_inodes_read, objects_with_subobjects
    );
    if !type_histogram.is_empty() {
        let mut types: Vec<(u16, u64)> = type_histogram.into_iter().collect();
        types.sort_by(|a, b| b.1.cmp(&a.1));
        println!("Object types seen (obj_type: count):");
        for (obj_type, count) in types.iter().take(25) {
            println!("  {:>3}: {}", obj_type, count);
        }
    }

    (recordings, programs, stations)
}

/// Resolve an absolute MFS path (e.g. `/Recording/NowShowing`) to an fsid by
/// walking the directory tree from the root.
fn resolve_path(drive: &mut TivoDrive, path: &str, input_path: &str) -> Option<u32> {
    let mut fsid = drive.volume_header.root_fsid;
    for component in path.split('/').filter(|c| !c.is_empty()) {
        let inode = drive.get_inode_from_fsid(fsid).ok()?;
        let entries = inode
            .get_entries_from_directory(input_path.to_string())
            .ok()?;
        fsid = entries.iter().find(|e| e.name == component)?.fsid;
    }
    Some(fsid)
}

/// Use the `/Recording` index directories to find recording fsids directly, then
/// follow each recording to its Program and Station. Much faster than a full
/// scan, but depends on an intact directory structure.
fn collect_via_now_showing(
    drive: &mut TivoDrive,
    input_path: &str,
) -> Option<(
    Vec<Recording>,
    HashMap<u32, ProgramInfo>,
    HashMap<u32, StationInfo>,
)> {
    // Names vary across software versions; try them in order of preference.
    const INDEX_PATHS: &[&str] = &[
        "/Recording/NowShowingByClassic",
        "/Recording/NowShowing",
        "/Recording/Complete",
    ];

    let dir_fsid = INDEX_PATHS
        .iter()
        .find_map(|path| resolve_path(drive, path, input_path))?;

    let dir_inode = drive.get_inode_from_fsid(dir_fsid).ok()?;
    let entries = dir_inode
        .get_entries_from_directory(input_path.to_string())
        .ok()?;

    let mut recordings = Vec::new();
    let mut programs: HashMap<u32, ProgramInfo> = HashMap::new();
    let mut stations: HashMap<u32, StationInfo> = HashMap::new();

    for entry in entries {
        let object = match read_object(drive, entry.fsid, input_path) {
            Some(object) => object,
            None => continue,
        };
        let recording = match recording_from_object(entry.fsid, &object) {
            Some(recording) => recording,
            None => continue,
        };

        if let Some(program_fsid) = recording.program_fsid {
            if let std::collections::hash_map::Entry::Vacant(slot) = programs.entry(program_fsid) {
                if let Some(program) = read_object(drive, program_fsid, input_path)
                    .as_ref()
                    .and_then(program_info)
                {
                    slot.insert(program);
                }
            }
        }
        if let Some(station_fsid) = recording.station_fsid {
            if let std::collections::hash_map::Entry::Vacant(slot) = stations.entry(station_fsid) {
                if let Some(station) = read_object(drive, station_fsid, input_path)
                    .as_ref()
                    .and_then(station_info)
                {
                    slot.insert(station);
                }
            }
        }

        recordings.push(recording);
    }

    if recordings.is_empty() {
        None
    } else {
        Some((recordings, programs, stations))
    }
}

pub fn run(input_path: &str, force_scan: bool) {
    println!("Loading TiVo Drive...");
    let mut drive =
        TivoDrive::from_disk_image(input_path).expect("Could not load TiVo drive");

    let collected = if force_scan {
        None
    } else {
        println!("Looking for recordings via the /Recording index...");
        collect_via_now_showing(&mut drive, input_path)
    };

    let (mut recordings, programs, stations) = match collected {
        Some(result) => result,
        None => {
            println!("Scanning all inodes for recordings (this can take a while)...");
            scan_all_inodes(&mut drive, input_path)
        }
    };

    // Most recent first.
    recordings.sort_by(|a, b| {
        (b.date, b.time)
            .cmp(&(a.date, a.time))
            .then(a.fsid.cmp(&b.fsid))
    });

    let mut table = Table::new();
    table.add_row(row![
        "FSID",
        "Title",
        "Episode",
        "Recorded",
        "Channel",
        "Orig. Air Date",
        "Description"
    ]);

    for recording in &recordings {
        let program = recording
            .program_fsid
            .and_then(|fsid| programs.get(&fsid))
            .cloned()
            .unwrap_or_default();
        let station = recording
            .station_fsid
            .and_then(|fsid| stations.get(&fsid))
            .cloned()
            .unwrap_or_default();

        let channel = station
            .call_sign
            .or(station.name)
            .unwrap_or_default();

        table.add_row(row![
            recording.fsid,
            truncate(program.title.as_deref().unwrap_or("<unknown>"), 40),
            truncate(program.episode_title.as_deref().unwrap_or(""), 30),
            tivo_date_time(recording.date, recording.time),
            channel,
            tivo_date(program.original_air_date),
            truncate(program.description.as_deref().unwrap_or(""), 60),
        ]);

        // Silence unused-field warnings for metadata we parse but don't tabulate.
        let _ = (recording.stream_size, program.movie_year);
    }

    table.printstd();
    println!("\n{} recording(s) found.", recordings.len());
}
