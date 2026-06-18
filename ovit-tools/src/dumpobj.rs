//! `dumpobj` subcommand: a diagnostic for the MFS database object format.
//!
//! Use it to inspect the raw bytes (and attempted parse) of a single object,
//! either by fsid or by searching every small inode for one whose data contains
//! a known string (e.g. a recording title found with `strings`). This is how we
//! calibrate the object parser against a real drive when titles aren't being
//! decoded as expected.

use ovit::TivoDrive;
use tivo_media_file_system::{MFSAttrValue, MFSObject};

/// Skip anything bigger than this so we never pull a media stream into memory.
const MAX_OBJECT_SIZE: u32 = 1 << 20;

fn hex_dump(data: &[u8], max_bytes: usize) {
    let limit = data.len().min(max_bytes);
    for (offset, chunk) in data[..limit].chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{:02x}", b)).collect();
        let ascii: String = chunk
            .iter()
            .map(|b| {
                if b.is_ascii_graphic() || *b == b' ' {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("  {:08x}  {:<47}  {}", offset * 16, hex.join(" "), ascii);
    }
    if data.len() > limit {
        println!("  ... ({} more bytes)", data.len() - limit);
    }
}

fn summarize_value(value: &MFSAttrValue) -> String {
    match value {
        MFSAttrValue::Int(values) => format!("int {:?}", values),
        MFSAttrValue::File(values) => format!("file {:?}", values),
        MFSAttrValue::Object(refs) => format!("object {:?}", refs),
        MFSAttrValue::Str(values) => format!("string {:?}", values),
    }
}

fn print_object(data: &[u8]) {
    if data.len() >= 8 {
        let fill1 = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        println!(
            "  object header: fill1={} size={} (buffer is {} bytes)",
            fill1,
            size,
            data.len()
        );
    }

    let object = MFSObject::parse(data);
    if object.subobjects.is_empty() {
        println!("  (no subobjects parsed)");
        return;
    }
    for subobject in &object.subobjects {
        println!(
            "  subobject obj_type={} flags={:#06x} id={} ({} attributes)",
            subobject.obj_type,
            subobject.flags,
            subobject.id,
            subobject.attributes.len()
        );
        for attr in &subobject.attributes {
            println!(
                "    attr id={:<3} eltype={:#04x} -> {}",
                attr.id,
                attr.eltype,
                summarize_value(&attr.value)
            );
        }
    }
}

fn dump_one(drive: &mut TivoDrive, fsid: u32, input_path: &str) {
    let inode = match drive.get_inode_from_fsid(fsid) {
        Ok(inode) => inode,
        Err(err) => {
            println!("Could not read inode for fsid {}: {}", fsid, err);
            return;
        }
    };
    println!(
        "fsid={} inode={} type={:?} size={} flags={:#x} numblocks={}",
        inode.fsid, inode.inode, inode.r#type, inode.size, inode.flags, inode.numblocks
    );
    let data = inode.get_data(input_path.to_string()).unwrap_or_default();
    hex_dump(&data, 512);
    print_object(&data);
}

fn find(drive: &mut TivoDrive, needle: &str, input_path: &str) {
    let needle = needle.as_bytes();
    let inodes = match drive.raw_zonemap.inode_iter() {
        Ok(iter) => iter,
        Err(err) => {
            println!("Could not iterate inodes: {}", err);
            return;
        }
    };

    let mut matches = 0;
    let mut scanned: u64 = 0;
    for inode in inodes {
        scanned += 1;
        if scanned % 50_000 == 0 {
            println!("  ...scanned {} inodes ({} matches)", scanned, matches);
        }
        // No type filter here — we don't trust our type assumptions yet. The
        // size cap keeps us out of media streams.
        if inode.fsid == 0 || inode.size == 0 || inode.size > MAX_OBJECT_SIZE {
            continue;
        }
        let data = match inode.get_data(input_path.to_string()) {
            Ok(data) => data,
            Err(_) => continue,
        };
        if data.len() < needle.len()
            || !data.windows(needle.len()).any(|window| window == needle)
        {
            continue;
        }

        println!("\n================ MATCH ================");
        println!(
            "fsid={} inode={} type={:?} size={} flags={:#x} numblocks={}",
            inode.fsid, inode.inode, inode.r#type, inode.size, inode.flags, inode.numblocks
        );
        hex_dump(&data, 1024);
        print_object(&data);

        matches += 1;
        if matches >= 5 {
            println!("\n(stopping after 5 matches)");
            break;
        }
    }

    println!("\n{} match(es) found.", matches);
}

pub fn run(input_path: &str, fsid: Option<u32>, needle: Option<&str>) {
    println!("Loading TiVo Drive...");
    let mut drive = TivoDrive::from_disk_image(input_path).expect("Could not load TiVo drive");

    match (fsid, needle) {
        (Some(fsid), _) => dump_one(&mut drive, fsid, input_path),
        (None, Some(needle)) => find(&mut drive, needle, input_path),
        (None, None) => {
            println!("Provide --fsid <N> to dump one object, or --find <STRING> to search.");
        }
    }
}
