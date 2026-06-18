//! `titles` subcommand: list recording titles by scanning the MFS application
//! regions for TiVo's NowShowing index strings.
//!
//! TiVo stores sortable index entries as plain text of the form
//! `<n>:<TITLE>:<n>:<n>:<fsid>` in the application region. These contain both
//! the human-readable title and the recording's fsid, so scanning for them is a
//! very robust way to enumerate recordings — it sidesteps the database object
//! format and the inode/zone traversal entirely, and still works on a partial or
//! damaged image.
//!
//! We read the raw partition bytes directly (no byte-order correction): the
//! index text is stored in natural order, which is how a plain `strings` pass
//! finds it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use ovit::TivoDrive;
use regex::bytes::Regex;

const SECTOR_BYTES: u64 = 512;
const CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Carry-over between chunks so a match spanning a chunk boundary is not lost.
/// Comfortably larger than the longest possible match.
const OVERLAP_BYTES: usize = 512;
/// Partitions larger than this (in 512-byte sectors) are treated as media
/// regions and skipped unless `--all` is given. ~4 GiB.
const APP_REGION_MAX_SECTORS: u32 = 8_000_000;

/// Number of *distinct* ASCII letters required for a title to be considered
/// real. Filters out junk like "ffff"/"ffffffff" (one distinct letter) while
/// keeping real titles.
const MIN_DISTINCT_LETTERS: usize = 3;

fn distinct_letters(bytes: &[u8]) -> usize {
    let mut seen = [false; 26];
    for &byte in bytes {
        if byte.is_ascii_alphabetic() {
            seen[(byte.to_ascii_lowercase() - b'a') as usize] = true;
        }
    }
    seen.iter().filter(|s| **s).count()
}

fn scan_range(
    file: &mut File,
    start: u64,
    length: u64,
    pattern: &Regex,
    out: &mut HashMap<u64, String>,
) {
    if file.seek(SeekFrom::Start(start)).is_err() {
        eprintln!("  could not seek to byte offset {}", start);
        return;
    }

    let mut remaining = length;
    let mut carry: Vec<u8> = Vec::new();

    while remaining > 0 {
        let want = CHUNK_BYTES.min(remaining as usize);
        let mut buf = vec![0u8; want];
        let read = match file.read(&mut buf) {
            Ok(0) => break, // EOF — common with a partial image
            Ok(read) => read,
            Err(err) => {
                eprintln!("  read error: {}", err);
                break;
            }
        };
        buf.truncate(read);
        remaining -= read as u64;

        // Prepend the previous tail so boundary-spanning matches are caught.
        let mut data = std::mem::take(&mut carry);
        data.extend_from_slice(&buf);

        for caps in pattern.captures_iter(&data) {
            let title_bytes = caps.get(1).unwrap().as_bytes();
            if distinct_letters(title_bytes) < MIN_DISTINCT_LETTERS {
                continue;
            }
            let fsid: u64 = match std::str::from_utf8(caps.get(2).unwrap().as_bytes())
                .ok()
                .and_then(|s| s.parse().ok())
            {
                Some(fsid) => fsid,
                None => continue,
            };
            let title = String::from_utf8_lossy(title_bytes).trim().to_string();
            if title.is_empty() {
                continue;
            }
            // Keep the longest title seen for a given recording — boundary or
            // partial matches yield shorter fragments of the same title.
            out.entry(fsid)
                .and_modify(|existing| {
                    if title.len() > existing.len() {
                        *existing = title.clone();
                    }
                })
                .or_insert(title);
        }

        let keep = data.len().min(OVERLAP_BYTES);
        carry = data[data.len() - keep..].to_vec();
    }
}

pub fn run(input_path: &str, scan_all: bool) {
    println!("Loading TiVo Drive...");
    let drive = TivoDrive::from_disk_image(input_path).expect("Could not load TiVo drive");

    let mfs_partitions: Vec<_> = drive
        .partition_map
        .partitions
        .iter()
        .filter(|p| p.r#type == "MFS")
        .collect();

    // Prefer partitions explicitly named as application regions; otherwise fall
    // back to the smaller MFS partitions (the media regions are huge).
    let targets: Vec<_> = if scan_all {
        mfs_partitions.clone()
    } else {
        let named: Vec<_> = mfs_partitions
            .iter()
            .copied()
            .filter(|p| p.name.to_lowercase().contains("application"))
            .collect();
        if named.is_empty() {
            mfs_partitions
                .iter()
                .copied()
                .filter(|p| p.sector_size <= APP_REGION_MAX_SECTORS)
                .collect()
        } else {
            named
        }
    };

    if targets.is_empty() {
        println!("No MFS application partitions found to scan. Partitions present:");
        for p in &mfs_partitions {
            println!("  '{}' type={} sectors={}", p.name, p.r#type, p.sector_size);
        }
        println!("Try again with --all to scan every MFS partition.");
        return;
    }

    // `\d+:<title>:\d+:\d+:<fsid>` over raw bytes (unicode mode off).
    let pattern = Regex::new(r"(?-u)\d+:([^:\x00]{1,90}):\d+:\d+:(\d+)")
        .expect("valid regex");

    let mut file = File::open(input_path).expect("Could not open image");
    let mut found: HashMap<u64, String> = HashMap::new();

    for partition in &targets {
        let start = u64::from(partition.starting_sector) * SECTOR_BYTES;
        let length = u64::from(partition.sector_size) * SECTOR_BYTES;
        println!(
            "Scanning '{}' ({} MiB) at sector {}...",
            partition.name,
            length / (1024 * 1024),
            partition.starting_sector
        );
        scan_range(&mut file, start, length, &pattern, &mut found);
    }

    let mut results: Vec<(u64, String)> = found.into_iter().collect();
    results.sort_by(|a, b| {
        a.1.to_uppercase()
            .cmp(&b.1.to_uppercase())
            .then(a.0.cmp(&b.0))
    });

    println!();
    for (fsid, title) in &results {
        println!("{:50}  fsid {}", title, fsid);
    }
    println!("\n{} unique recordings", results.len());
}
