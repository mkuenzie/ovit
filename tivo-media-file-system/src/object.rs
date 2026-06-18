//! Parser for the TiVo MFS database "object" format (a.k.a. tyDB objects).
//!
//! The filesystem layer (inodes + directory entries) only tells us where bytes
//! live. The actual program/recording *metadata* — titles, descriptions, air
//! dates, etc. — is stored as the data of "database" (Db) inodes, encoded in the
//! object format described below. The field meanings (which `attr` id maps to
//! "Title", etc.) come from `schema.txt`.
//!
//! On-disk layout (all multi-byte integers big-endian, matching the rest of the
//! crate after byte-order correction):
//!
//! ```text
//! object:
//!   u32 fill1
//!   u32 size            // total object length in bytes
//!   repeated subobjects until `size` is reached
//!
//! subobject (16 byte header):
//!   u16 len             // total subobject length, including this header
//!   u16 len1
//!   u16 obj_type        // schema object type (3=Program, 14=Recording, ...)
//!   u16 flags
//!   u16 fill[2]
//!   u32 id
//!   ... attributes fill the remaining (len - 16) bytes
//!
//! attribute (4 byte header):
//!   u8  eltype          // (eltype >> 6) selects the value type
//!   u8  attr            // schema field id (17=Title on a Program, ...)
//!   u16 len             // total attribute length, including this header
//!   ... value bytes, then padded so the next attribute is 4-byte aligned
//! ```
//!
//! Reference: `elitak/mfs-utils` (`object.c`, `attribute.h`, `generate_NowShowing.c`).

/// Schema object type numbers we care about (see `schema.txt`).
pub mod obj_type {
    pub const PROGRAM: u16 = 3;
    pub const SERIES: u16 = 4;
    pub const STATION: u16 = 5;
    pub const SHOWING: u16 = 7;
    pub const RECORDING: u16 = 14;
}

/// The decoded value of an attribute. `eltype >> 6` selects which of these it is.
#[derive(Debug, Clone)]
pub enum MFSAttrValue {
    /// `TYPE_INT` (0): one or more big-endian `u32`s.
    Int(Vec<u32>),
    /// `TYPE_STRING` (1): one or more NUL-terminated strings.
    Str(Vec<String>),
    /// `TYPE_OBJECT` (2): references to other objects, as `(fsid, subobj)` pairs.
    Object(Vec<(u32, u32)>),
    /// `TYPE_FILE` (3): file references, stored like ints.
    File(Vec<u32>),
}

#[derive(Debug, Clone)]
pub struct MFSAttribute {
    pub id: u8,
    pub eltype: u8,
    pub value: MFSAttrValue,
}

#[derive(Debug, Clone)]
pub struct MFSSubobject {
    pub obj_type: u16,
    pub flags: u16,
    pub id: u32,
    pub attributes: Vec<MFSAttribute>,
}

#[derive(Debug, Clone)]
pub struct MFSObject {
    pub subobjects: Vec<MFSSubobject>,
}

fn be_u16(buf: &[u8], ofs: usize) -> u16 {
    u16::from_be_bytes([buf[ofs], buf[ofs + 1]])
}

fn be_u32(buf: &[u8], ofs: usize) -> u32 {
    u32::from_be_bytes([buf[ofs], buf[ofs + 1], buf[ofs + 2], buf[ofs + 3]])
}

fn decode_value(eltype: u8, data: &[u8]) -> MFSAttrValue {
    match eltype >> 6 {
        1 => {
            // Strings are packed and NUL terminated. Empty trailing slices (from
            // the final terminator) are dropped.
            let strings = data
                .split(|b| *b == 0)
                .filter(|chunk| !chunk.is_empty())
                .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                .collect();
            MFSAttrValue::Str(strings)
        }
        2 => {
            let mut refs = Vec::new();
            let mut i = 0;
            while i + 8 <= data.len() {
                refs.push((be_u32(data, i), be_u32(data, i + 4)));
                i += 8;
            }
            MFSAttrValue::Object(refs)
        }
        3 | 0 => {
            let mut ints = Vec::new();
            let mut i = 0;
            while i + 4 <= data.len() {
                ints.push(be_u32(data, i));
                i += 4;
            }
            if eltype >> 6 == 3 {
                MFSAttrValue::File(ints)
            } else {
                MFSAttrValue::Int(ints)
            }
        }
        _ => unreachable!("eltype >> 6 is always 0..=3"),
    }
}

fn parse_attributes(buf: &[u8]) -> Vec<MFSAttribute> {
    let mut attrs = Vec::new();
    let mut ofs = 0;
    while ofs + 4 <= buf.len() {
        let eltype = buf[ofs];
        let id = buf[ofs + 1];
        let len = be_u16(buf, ofs + 2) as usize;
        // A length smaller than the header, or zero, would never terminate the
        // loop — bail out instead of spinning or panicking on corrupt data.
        if len < 4 {
            break;
        }
        let data_end = (ofs + len).min(buf.len());
        let value = decode_value(eltype, &buf[ofs + 4..data_end]);
        attrs.push(MFSAttribute { id, eltype, value });
        ofs += (len + 3) & !3;
    }
    attrs
}

impl MFSObject {
    /// Parse an object buffer (the raw data of a Db inode). This is deliberately
    /// tolerant: malformed or truncated buffers yield whatever could be parsed
    /// rather than an error, so a partially-recovered drive still surfaces data.
    pub fn parse(buf: &[u8]) -> MFSObject {
        let mut subobjects = Vec::new();
        if buf.len() < 8 {
            return MFSObject { subobjects };
        }

        let size = be_u32(buf, 4) as usize;
        let end = size.min(buf.len());
        let mut ofs = 8;

        while ofs + 16 <= end {
            let len = be_u16(buf, ofs) as usize;
            let obj_type = be_u16(buf, ofs + 4);
            let flags = be_u16(buf, ofs + 6);
            let id = be_u32(buf, ofs + 12);

            // Guard against corrupt lengths that would loop forever or run off
            // the end of the buffer.
            if len < 16 || ofs + len > end {
                break;
            }

            let attributes = parse_attributes(&buf[ofs + 16..ofs + len]);
            subobjects.push(MFSSubobject {
                obj_type,
                flags,
                id,
                attributes,
            });
            ofs += len;
        }

        MFSObject { subobjects }
    }

    pub fn has_type(&self, obj_type: u16) -> bool {
        self.subobjects.iter().any(|s| s.obj_type == obj_type)
    }

    pub fn first_of_type(&self, obj_type: u16) -> Option<&MFSSubobject> {
        self.subobjects.iter().find(|s| s.obj_type == obj_type)
    }
}

impl MFSSubobject {
    pub fn attribute(&self, id: u8) -> Option<&MFSAttribute> {
        self.attributes.iter().find(|a| a.id == id)
    }

    /// First string value of a string attribute.
    pub fn string(&self, id: u8) -> Option<String> {
        match &self.attribute(id)?.value {
            MFSAttrValue::Str(values) => values.first().cloned(),
            _ => None,
        }
    }

    /// All string values of a string attribute.
    pub fn strings(&self, id: u8) -> Vec<String> {
        match self.attribute(id).map(|a| &a.value) {
            Some(MFSAttrValue::Str(values)) => values.clone(),
            _ => Vec::new(),
        }
    }

    /// First integer value of an int attribute.
    pub fn int(&self, id: u8) -> Option<u32> {
        match &self.attribute(id)?.value {
            MFSAttrValue::Int(values) | MFSAttrValue::File(values) => values.first().copied(),
            _ => None,
        }
    }

    /// First referenced fsid of an object attribute.
    pub fn object_fsid(&self, id: u8) -> Option<u32> {
        match &self.attribute(id)?.value {
            MFSAttrValue::Object(refs) => refs.first().map(|(fsid, _)| *fsid),
            _ => None,
        }
    }
}
