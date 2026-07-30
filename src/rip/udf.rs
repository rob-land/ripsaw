//! Minimal read-only UDF reader — enough to pull BD-ROM structures (playlists,
//! SSIF / m2ts streams) straight out of a Blu-ray `.iso`, **including the UDF
//! 2.50 Metadata Partition** that Blu-ray uses.
//!
//! Why this exists: a 3D Blu-ray image has to be read to (a) find the 3D feature
//! playlist and (b) stream the interleaved SSIF into the MVC decoder. The obvious
//! route — loop-mount the ISO and walk the filesystem — fails inside a Flatpak
//! sandbox, because the `udisksctl` mount lands on `/run/media` on the *host* and
//! that late mount isn't visible in our mount namespace. Reading the ISO file
//! directly (which we *can* do via the portal) sidesteps the mount entirely.
//!
//! Scope: read-only, big-endian-free (UDF is little-endian), no allocation of
//! whole files — [`Udf::extents`] hands back physical byte ranges so a 40 GB SSIF
//! streams through [`ExtentReader`] without ever being buffered. Only what
//! BD-ROM actually uses is implemented: physical (Type 1) + metadata (Type 2)
//! partitions, (Extended) File Entries, short/long allocation descriptors with
//! continuation, and directory FID traversal. Sparable/Virtual partitions and
//! Named Streams are not handled (BD-ROM doesn't use them for this purpose).
//!
//! Layout constants were cross-checked against the ECMA-167 / OSTA UDF specs and
//! validated byte-exact against a real 3D Blu-ray image (see the `udf_ls`
//! example and the `RIPSAW_TEST_ISO` integration test).

use std::io::{self, Read, Seek, SeekFrom};

const SECTOR: u64 = 2048;

// Descriptor tag identifiers (ECMA-167 3/7.2.1, 4/7.2.1).
const TAG_AVDP: u16 = 2; // Anchor Volume Descriptor Pointer
const TAG_PD: u16 = 5; // Partition Descriptor
const TAG_LVD: u16 = 6; // Logical Volume Descriptor
const TAG_FSD: u16 = 256; // File Set Descriptor
const TAG_FID: u16 = 257; // File Identifier Descriptor
const TAG_FE: u16 = 261; // File Entry
const TAG_FE_BASE: u16 = 260; // File Entry (older tag value seen in the wild)
const TAG_EFE: u16 = 266; // Extended File Entry

// ICB allocation-descriptor type (ICB Tag Flags & 0x7, ECMA-167 4/14.6.8).
const ALLOC_SHORT: u16 = 0;
const ALLOC_LONG: u16 = 1;
const ALLOC_INLINE: u16 = 3;

// Extent kind, held in the top 2 bits of an allocation descriptor's length
// (ECMA-167 4/14.14.1.1). 0 = recorded+allocated (real data); 1/2 = allocated
// but not recorded (read as zeros); 3 = the extent points to a continuation
// Allocation Extent Descriptor.
const EXT_RECORDED: u32 = 0;
const EXT_CONTINUATION: u32 = 3;

// File Characteristics bits (ECMA-167 4/14.4.3).
const FC_DELETED: u8 = 0x04;
const FC_PARENT: u8 = 0x08;
const FC_DIRECTORY: u8 = 0x02;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

fn err(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("UDF: {msg}"))
}

/// Which partition a logical block number is expressed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    /// Type 1 physical partition: physical LBA = `part_start + lbn`.
    Phys,
    /// Type 2 metadata partition: resolved through the metadata file's extents.
    Meta,
}

/// A physical byte range in the ISO. `offset: None` means "allocated but not
/// recorded" — read as zeros (never happens for BD-ROM data files, but modelled
/// for correctness).
#[derive(Clone, Copy, Debug)]
pub struct PhysExtent {
    pub offset: Option<u64>,
    pub len: u64,
}

/// A resolved directory entry.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    fe_lbn: u32,
    fe_part: Part,
}

/// The parsed volume layout — cheap to hold, carries no reader.
pub struct Udf {
    part_start: u32,
    /// Metadata file data extents, as `(partition-relative lbn, byte len)` pairs
    /// in the physical partition. Empty for a plain physical-only volume.
    meta_extents: Vec<(u32, u64)>,
    /// Partition-map index of the Type 2 metadata partition, if present. A
    /// long_ad / FID whose partition reference equals this addresses metadata;
    /// anything else addresses the physical partition.
    meta_map_ref: Option<u16>,
    root_fe_lbn: u32,
    root_fe_part: Part,
}

impl Udf {
    /// Parse the volume: AVDP → VDS (Partition + Logical Volume descriptors) →
    /// metadata file (if any) → File Set Descriptor → root directory ICB.
    pub fn open<R: Read + Seek>(r: &mut R) -> io::Result<Udf> {
        // Anchor Volume Descriptor Pointer sits at logical sector 256.
        let avdp = read_sector(r, 256)?;
        if u16le(&avdp, 0) != TAG_AVDP {
            return Err(err("no Anchor Volume Descriptor Pointer at sector 256 (not a UDF image?)"));
        }
        let vds_len = u32le(&avdp, 16) as u64;
        let vds_loc = u32le(&avdp, 20);

        // Walk the Main Volume Descriptor Sequence for the Partition Descriptor
        // and Logical Volume Descriptor.
        let mut pd: Option<Vec<u8>> = None;
        let mut lvd: Option<Vec<u8>> = None;
        let n = vds_len.div_ceil(SECTOR) as u32;
        for i in 0..n {
            let d = read_sector(r, (vds_loc + i) as u64)?;
            match u16le(&d, 0) {
                TAG_PD => pd = Some(d),
                TAG_LVD => lvd = Some(d),
                _ => {}
            }
        }
        let pd = pd.ok_or_else(|| err("no Partition Descriptor in VDS"))?;
        let lvd = lvd.ok_or_else(|| err("no Logical Volume Descriptor in VDS"))?;

        let part_start = u32le(&pd, 188);

        // Logical Volume Descriptor: File Set Descriptor long_ad lives in the
        // 16-byte LogicalVolumeContentsUse @248; partition maps @440.
        let fsd_lbn = u32le(&lvd, 252);
        let fsd_part_ref = u16le(&lvd, 256);
        let n_maps = u32le(&lvd, 268) as usize;
        let map_table_len = u32le(&lvd, 264) as usize;

        // Parse partition maps. Type 1 (physical) is map ref 0-style; Type 2
        // "*UDF Metadata Partition" carries the metadata-file location.
        let mut meta_file_lbn: Option<u32> = None;
        let mut phys_map_ref: Option<u16> = None;
        let mut meta_map_ref: Option<u16> = None;
        {
            let maps_end = (440 + map_table_len).min(lvd.len());
            let mut off = 440usize;
            let mut idx = 0u16;
            while (idx as usize) < n_maps && off + 2 <= maps_end {
                let map_type = lvd[off];
                let map_len = lvd[off + 1] as usize;
                if map_len < 2 || off + map_len > maps_end {
                    break;
                }
                let map = &lvd[off..off + map_len];
                match map_type {
                    1 if map_len >= 6 => phys_map_ref = Some(idx),
                    2 if map_len >= 44 => {
                        // Type 2 metadata partition map: reserved(2), partition
                        // type regid(32) with identifier at map[5..], then
                        // volSeq(2)@36, partition#(2)@38, metadataFileLoc(4)@40.
                        let ident = &map[5..(5 + 23).min(map.len())];
                        if ident.starts_with(b"*UDF Metadata Partition") {
                            meta_file_lbn = Some(u32le(map, 40));
                            meta_map_ref = Some(idx);
                        }
                    }
                    _ => {}
                }
                off += map_len;
                idx += 1;
            }
        }

        // The physical partition is the default when a map ref selects Type 1.
        let phys_ref = phys_map_ref.unwrap_or(0);
        let part_of = |map_ref: u16| -> Part {
            if Some(map_ref) == meta_map_ref {
                Part::Meta
            } else if map_ref == phys_ref {
                Part::Phys
            } else {
                // Unknown map refs default to physical; BD-ROM only has the two.
                Part::Phys
            }
        };

        // Bootstrap the metadata file (its File Entry lives in the physical
        // partition at part_start + metaFileLbn), yielding the logical→physical
        // block map for everything addressed in the metadata partition.
        let mut udf = Udf {
            part_start,
            meta_extents: Vec::new(),
            meta_map_ref,
            root_fe_lbn: 0,
            root_fe_part: Part::Phys,
        };
        if let Some(mfl) = meta_file_lbn {
            let fe = udf.read_fe(r, mfl, Part::Phys)?;
            udf.meta_extents = fe
                .extents
                .iter()
                .filter_map(|e| e.offset.map(|off| {
                    // Convert the physical byte offset back to a partition-
                    // relative lbn for the metadata map.
                    let lbn = (off / SECTOR) as u32 - part_start;
                    (lbn, e.len)
                }))
                .collect();
        }

        // File Set Descriptor → root directory ICB (long_ad @400: len@400,
        // lbn@404, partRef@408).
        let fsd_part = part_of(fsd_part_ref);
        let fsd_phys = udf
            .resolve(fsd_lbn, fsd_part)
            .ok_or_else(|| err("cannot resolve File Set Descriptor"))?;
        let fsd = read_sector(r, fsd_phys)?;
        if u16le(&fsd, 0) != TAG_FSD {
            return Err(err("File Set Descriptor tag mismatch"));
        }
        udf.root_fe_lbn = u32le(&fsd, 404);
        udf.root_fe_part = part_of(u16le(&fsd, 408));
        Ok(udf)
    }

    /// Physical LBA for a logical block in the given partition.
    fn resolve(&self, lbn: u32, part: Part) -> Option<u64> {
        match part {
            Part::Phys => Some(self.part_start as u64 + lbn as u64),
            Part::Meta => {
                let mut blk = 0u64;
                for &(mlbn, len) in &self.meta_extents {
                    let nblk = len.div_ceil(SECTOR);
                    if (lbn as u64) < blk + nblk {
                        return Some(self.part_start as u64 + mlbn as u64 + (lbn as u64 - blk));
                    }
                    blk += nblk;
                }
                None
            }
        }
    }

    /// The root directory as an [`Entry`]-like handle usable with [`read_dir`].
    fn root(&self) -> (u32, Part) {
        (self.root_fe_lbn, self.root_fe_part)
    }

    /// Read a (Extended) File Entry and resolve its allocation descriptors into
    /// physical byte extents. `home` is the partition the FE resides in — the
    /// implicit partition for `short_ad` descriptors.
    fn read_fe<R: Read + Seek>(&self, r: &mut R, lbn: u32, home: Part) -> io::Result<Fe> {
        let phys = self
            .resolve(lbn, home)
            .ok_or_else(|| err("cannot resolve File Entry location"))?;
        let sector = read_sector(r, phys)?;
        let tag = u16le(&sector, 0);
        let is_efe = tag == TAG_EFE;
        if tag != TAG_FE && tag != TAG_FE_BASE && !is_efe {
            return Err(err("expected a File Entry"));
        }
        let icb_flags = u16le(&sector, 34);
        let alloc = icb_flags & 0x0007;
        let info_len = u64le(&sector, 56);

        // Base FE (4/14.9): L_EA@168, L_AD@172, area@176. Extended FE (4/14.17)
        // inserts 40 bytes (ObjectSize, times, StreamDir ICB, reserved) so the
        // lengths shift to L_EA@208, L_AD@212, area@216.
        let (ea_off, ad_off, header) = if is_efe { (208, 212, 216) } else { (168, 172, 176) };
        let ea_len = u32le(&sector, ea_off) as usize;
        let ad_len = u32le(&sector, ad_off) as usize;
        let ad_start = header + ea_len;
        let ad_end = ad_start + ad_len;
        if ad_end > sector.len() {
            return Err(err("allocation descriptor area overruns the File Entry sector"));
        }
        let ad_area = sector[ad_start..ad_end].to_vec();

        let mut fe = Fe {
            is_dir: false,
            size: info_len,
            extents: Vec::new(),
            inline: None,
        };
        match alloc {
            ALLOC_INLINE => {
                let end = (info_len as usize).min(ad_area.len());
                fe.inline = Some(ad_area[..end].to_vec());
            }
            ALLOC_SHORT => self.collect_extents(r, &ad_area, home, false, &mut fe.extents)?,
            ALLOC_LONG => self.collect_extents(r, &ad_area, home, true, &mut fe.extents)?,
            _ => return Err(err("unknown ICB allocation type")),
        }
        Ok(fe)
    }

    /// Parse a run of short_ad (8-byte) or long_ad (16-byte) descriptors,
    /// following any type-3 continuation into an Allocation Extent Descriptor.
    fn collect_extents<R: Read + Seek>(
        &self,
        r: &mut R,
        area: &[u8],
        home: Part,
        long: bool,
        out: &mut Vec<PhysExtent>,
    ) -> io::Result<()> {
        let step = if long { 16 } else { 8 };
        let mut area = area.to_vec();
        // Bound continuation-following so a malformed image can't spin forever.
        for _ in 0..4096 {
            let mut pos = 0usize;
            let mut continuation: Option<(u32, Part)> = None;
            while pos + step <= area.len() {
                let len_raw = u32le(&area, pos);
                let ext_type = len_raw >> 30;
                let ext_len = (len_raw & 0x3FFF_FFFF) as u64;
                let lbn = u32le(&area, pos + 4);
                let part = if long {
                    // long_ad partition reference @ pos+8 selects the map.
                    match u16le(&area, pos + 8) {
                        rf if Some(rf) == self.meta_map_ref => Part::Meta,
                        _ => Part::Phys,
                    }
                } else {
                    home
                };
                if ext_len == 0 {
                    pos += step;
                    continue;
                }
                match ext_type {
                    EXT_CONTINUATION => {
                        continuation = Some((lbn, part));
                        break;
                    }
                    EXT_RECORDED => {
                        let phys = self
                            .resolve(lbn, part)
                            .ok_or_else(|| err("cannot resolve data extent"))?;
                        out.push(PhysExtent {
                            offset: Some(phys * SECTOR),
                            len: ext_len,
                        });
                    }
                    _ => out.push(PhysExtent { offset: None, len: ext_len }),
                }
                pos += step;
            }
            let Some((clbn, cpart)) = continuation else { return Ok(()) };
            // Allocation Extent Descriptor: tag(16) + prevAllocExtLoc(4) +
            // L_AD(4), descriptors follow at offset 24.
            let phys = self
                .resolve(clbn, cpart)
                .ok_or_else(|| err("cannot resolve continuation extent"))?;
            let sector = read_sector(r, phys)?;
            let l_ad = u32le(&sector, 20) as usize;
            let end = (24 + l_ad).min(sector.len());
            area = sector[24..end].to_vec();
        }
        Err(err("allocation-descriptor continuation chain too long"))
    }

    /// List a directory's entries (parent `..` and deleted entries are skipped).
    fn read_dir<R: Read + Seek>(&self, r: &mut R, lbn: u32, part: Part) -> io::Result<Vec<Entry>> {
        let fe = self.read_fe(r, lbn, part)?;
        let data = self.read_all(r, &fe)?;
        let mut entries = Vec::new();
        let mut off = 0usize;
        while off + 38 <= data.len() {
            if u16le(&data, off) != TAG_FID {
                off += 4;
                continue;
            }
            let fc = data[off + 18];
            let l_fi = data[off + 19] as usize;
            let icb_lbn = u32le(&data, off + 24);
            let icb_ref = u16le(&data, off + 28);
            let l_iu = u16le(&data, off + 36) as usize;
            let total = 38 + l_iu + l_fi;
            let advance = (total + 3) & !3;
            if fc & FC_PARENT == 0 && fc & FC_DELETED == 0 {
                let id_start = off + 38 + l_iu;
                let id_end = (id_start + l_fi).min(data.len());
                let name = if id_end > id_start {
                    decode_dstring(&data[id_start..id_end])
                } else {
                    String::new()
                };
                let part = if Some(icb_ref) == self.meta_map_ref {
                    Part::Meta
                } else {
                    Part::Phys
                };
                // Fetch the child's size/kind from its File Entry.
                let (is_dir, size) = match self.read_fe(r, icb_lbn, part) {
                    Ok(cfe) => (fc & FC_DIRECTORY != 0, cfe.size),
                    Err(_) => (fc & FC_DIRECTORY != 0, 0),
                };
                entries.push(Entry {
                    name,
                    is_dir,
                    size,
                    fe_lbn: icb_lbn,
                    fe_part: part,
                });
            }
            off += advance.max(4);
        }
        Ok(entries)
    }

    /// Read an FE's full data into memory. Use only for small files (playlists);
    /// stream large files with [`extents`](Self::extents) + [`ExtentReader`].
    fn read_all<R: Read + Seek>(&self, r: &mut R, fe: &Fe) -> io::Result<Vec<u8>> {
        if let Some(inline) = &fe.inline {
            return Ok(inline.clone());
        }
        let mut out = Vec::with_capacity(fe.size as usize);
        for ext in &fe.extents {
            match ext.offset {
                Some(phys) => {
                    r.seek(SeekFrom::Start(phys))?;
                    let mut chunk = vec![0u8; ext.len as usize];
                    r.read_exact(&mut chunk)?;
                    out.extend_from_slice(&chunk);
                }
                None => out.resize(out.len() + ext.len as usize, 0),
            }
        }
        out.truncate(fe.size as usize);
        Ok(out)
    }

    /// Resolve a `/`-separated path to an entry (case-insensitive, as BD-ROM
    /// tools may vary case). Returns `Ok(None)` if any component is missing.
    pub fn lookup<R: Read + Seek>(&self, r: &mut R, path: &str) -> io::Result<Option<Entry>> {
        let (mut lbn, mut part) = self.root();
        let mut last: Option<Entry> = None;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let entries = self.read_dir(r, lbn, part)?;
            let Some(found) = entries
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(comp))
            else {
                return Ok(None);
            };
            lbn = found.fe_lbn;
            part = found.fe_part;
            last = Some(found);
        }
        Ok(last)
    }

    /// List the entries under a directory path (`""` or `"/"` = root).
    pub fn list<R: Read + Seek>(&self, r: &mut R, path: &str) -> io::Result<Vec<Entry>> {
        let (lbn, part) = match self.lookup(r, path)? {
            Some(e) if e.is_dir => (e.fe_lbn, e.fe_part),
            Some(_) => return Err(err("path is not a directory")),
            None if path.trim_matches('/').is_empty() => self.root(),
            None => return Ok(Vec::new()),
        };
        self.read_dir(r, lbn, part)
    }

    /// Read a whole (small) file by path into memory.
    pub fn read_file<R: Read + Seek>(&self, r: &mut R, path: &str) -> io::Result<Vec<u8>> {
        let e = self
            .lookup(r, path)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("UDF: no such file {path}")))?;
        let fe = self.read_fe(r, e.fe_lbn, e.fe_part)?;
        self.read_all(r, &fe)
    }

    /// The size and physical byte extents of a file, for streaming without
    /// buffering. Feed the result to [`ExtentReader::new`].
    pub fn extents<R: Read + Seek>(
        &self,
        r: &mut R,
        path: &str,
    ) -> io::Result<(u64, Vec<PhysExtent>)> {
        let e = self
            .lookup(r, path)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("UDF: no such file {path}")))?;
        let fe = self.read_fe(r, e.fe_lbn, e.fe_part)?;
        if let Some(inline) = fe.inline {
            // Tiny inline file: no physical extent — expose via a note-length of 0.
            return Ok((inline.len() as u64, vec![]));
        }
        Ok((fe.size, fe.extents))
    }
}

/// A parsed File Entry: either inline data or a list of physical byte extents.
struct Fe {
    #[allow(dead_code)]
    is_dir: bool,
    size: u64,
    extents: Vec<PhysExtent>,
    inline: Option<Vec<u8>>,
}

fn read_sector<R: Read + Seek>(r: &mut R, lba: u64) -> io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(lba * SECTOR))?;
    let mut buf = vec![0u8; SECTOR as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Decode an OSTA CS0 compressed d-string: leading byte 8 = 8-bit (Latin-1 /
/// UTF-8), 16 = UTF-16BE. A file identifier has no trailing length byte (unlike
/// a fixed d-string field), so decode the whole payload.
fn decode_dstring(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    match bytes[0] {
        16 => {
            let units: Vec<u16> = bytes[1..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(&bytes[1..]).into_owned(),
    }
}

/// A `Read`+`Seek` view over a file described by physical byte extents in the
/// underlying image. Streams arbitrarily large files (e.g. a 40 GB SSIF) without
/// buffering — each read maps the current position to the right extent and reads
/// straight from the image.
pub struct ExtentReader<R> {
    inner: R,
    extents: Vec<PhysExtent>,
    /// Cumulative logical start offset of each extent (same length as extents).
    starts: Vec<u64>,
    size: u64,
    pos: u64,
}

impl<R: Read + Seek> ExtentReader<R> {
    pub fn new(inner: R, size: u64, extents: Vec<PhysExtent>) -> Self {
        let mut starts = Vec::with_capacity(extents.len());
        let mut acc = 0u64;
        for e in &extents {
            starts.push(acc);
            acc += e.len;
        }
        ExtentReader {
            inner,
            extents,
            starts,
            size,
            pos: 0,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

impl<R: Read + Seek> Read for ExtentReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        // Find the extent containing `pos` (extents are contiguous & ordered).
        let idx = match self.starts.binary_search(&self.pos) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let ext = self.extents[idx];
        let into_ext = self.pos - self.starts[idx];
        let remaining_in_ext = ext.len - into_ext;
        let remaining_in_file = self.size - self.pos;
        let want = buf.len().min(remaining_in_ext.min(remaining_in_file) as usize);
        let n = match ext.offset {
            Some(phys) => {
                self.inner.seek(SeekFrom::Start(phys + into_ext))?;
                self.inner.read(&mut buf[..want])?
            }
            None => {
                // Unrecorded extent: yield zeros.
                for b in &mut buf[..want] {
                    *b = 0;
                }
                want
            }
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for ExtentReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.size as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_dstring_variants() {
        assert_eq!(decode_dstring(&[]), "");
        assert_eq!(decode_dstring(b"\x08BDMV"), "BDMV");
        // Compression id 16 = UTF-16BE.
        assert_eq!(decode_dstring(&[16, 0x00, 0x41, 0x00, 0x42]), "AB");
    }

    #[test]
    fn extent_reader_spans_extents_and_zeros() {
        // Two data extents ("AAAA","BBBB") + one unrecorded (zeros) extent.
        let mut img = vec![0u8; 100];
        img[10..14].copy_from_slice(b"AAAA");
        img[20..24].copy_from_slice(b"BBBB");
        let extents = vec![
            PhysExtent { offset: Some(10), len: 4 },
            PhysExtent { offset: Some(20), len: 4 },
            PhysExtent { offset: None, len: 2 },
        ];
        let mut rd = ExtentReader::new(io::Cursor::new(img), 10, extents);
        let mut out = Vec::new();
        rd.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"AAAABBBB\x00\x00");

        // Seek + partial read lands in the right extent.
        rd.seek(SeekFrom::Start(6)).unwrap();
        let mut two = [0u8; 2];
        rd.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"BB");
    }

    // Integration test against a real Blu-ray image. Ignored by default; run with
    //   RIPSAW_TEST_ISO=/path/to/bd.iso cargo test udf_real_iso -- --ignored --nocapture
    #[test]
    #[ignore]
    fn udf_real_iso() {
        let Ok(path) = std::env::var("RIPSAW_TEST_ISO") else {
            eprintln!("set RIPSAW_TEST_ISO to run");
            return;
        };
        let mut f = std::fs::File::open(&path).unwrap();
        let udf = Udf::open(&mut f).unwrap();
        let root: Vec<String> = udf.list(&mut f, "/").unwrap().iter().map(|e| e.name.clone()).collect();
        println!("root: {root:?}");
        assert!(root.iter().any(|n| n == "BDMV"));
        let pl = udf.list(&mut f, "BDMV/PLAYLIST").unwrap();
        println!("playlists: {:?}", pl.iter().map(|e| (&e.name, e.size)).collect::<Vec<_>>());
        assert!(pl.iter().any(|e| e.name.ends_with(".mpls")));
    }
}
