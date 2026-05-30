// Parse Blu-ray meta-files. The Blu-ray Disc spec (BDA) defines
// `BDMV/META/DL/bdmt_<lang>.xml` as an optional disc-level metadata
// file. When a publisher bothers to fill it in (most do not), it
// carries the disc title, the multi-disc set number / count, and
// sometimes per-title names. That's exactly the data the user is
// trying to enter by hand when submitting an unidentified disc to
// TheDiscDB.
//
// Schema, paraphrased from the BDA document (urn:BDA:bdmv;...):
//
//   <disclib xmlns="urn:BDA:bdmv;disclib">
//     <di:discinfo xmlns:di="urn:BDA:bdmv;discinfo">
//       <di:title>
//         <di:name>Skyfall</di:name>
//         <di:numSets>1</di:numSets>
//         <di:setNumber>1</di:setNumber>
//       </di:title>
//       <di:description>
//         <di:thumbnail href="thumbnail/eng_thumbnail.jpg" size="416x240" />
//       </di:description>
//     </di:discinfo>
//     <di:tableofcontents xmlns:di="urn:BDA:bdmv;tableofcontents">
//       <di:titles>
//         <di:title titleNumber="1">
//           <di:titleName>Main Movie</di:titleName>
//         </di:title>
//       </di:titles>
//     </di:tableofcontents>
//   </disclib>
//
// Real-world coverage is sparse: most discs leave the META directory
// empty or absent entirely. When the file IS present and populated
// it's reliable -- the publisher authored it. So Ripsaw treats this
// as "use when present, ignore otherwise" pre-fill data.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BdmtMetadata {
    /// `<di:name>` inside `<di:title>` -- the disc's display name.
    pub disc_title: Option<String>,
    /// `<di:numSets>` -- how many discs make up this set (1 for a
    /// standalone, 2 for a 2-disc set, etc.). Useful as a disc-level
    /// hint when an album/series is split across multiple discs.
    pub num_sets: Option<u32>,
    /// `<di:setNumber>` -- which disc in the set this is (1-based).
    pub set_number: Option<u32>,
    /// `<di:tableofcontents>` per-title names keyed by `titleNumber`
    /// attribute. Empty when the disc only carries the discinfo block.
    pub title_names: HashMap<u32, String>,
}

impl BdmtMetadata {
    pub fn is_empty(&self) -> bool {
        self.disc_title.is_none()
            && self.num_sets.is_none()
            && self.set_number.is_none()
            && self.title_names.is_empty()
    }
}

/// Look for `BDMV/META/DL/bdmt_eng.xml` (or other language variants
/// when English isn't present) under `mount` and parse it. Returns
/// `Ok(None)` when no meta file exists -- which is the common case;
/// callers should treat that as "no hint available" not as an error.
pub fn read_from_mount(mount: &Path) -> Result<Option<BdmtMetadata>> {
    let meta_dir = mount.join("BDMV").join("META").join("DL");
    if !meta_dir.is_dir() {
        return Ok(None);
    }

    // Prefer English; fall back to whatever's in the directory so a
    // non-English-only disc still surfaces something.
    let preferred = meta_dir.join("bdmt_eng.xml");
    let chosen = if preferred.is_file() {
        preferred
    } else {
        let mut first_xml = None;
        for entry in std::fs::read_dir(&meta_dir)?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml"))
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("bdmt_"))
            {
                first_xml = Some(path);
                break;
            }
        }
        match first_xml {
            Some(p) => p,
            None => return Ok(None),
        }
    };

    let text = std::fs::read_to_string(&chosen)?;
    Ok(Some(parse(&text)?))
}

/// Parse a bdmt XML payload into a `BdmtMetadata`. Pure -- no I/O.
pub fn parse(xml: &str) -> Result<BdmtMetadata> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut out = BdmtMetadata::default();
    // Element-path stack so we know which open element a text event
    // belongs to. Element names are stored stripped of any
    // `prefix:` namespace prefix.
    let mut path: Vec<String> = Vec::new();
    // When inside a <di:title titleNumber="N"> for the table-of-
    // contents, the current title number.
    let mut current_toc_title_number: Option<u32> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => {
                anyhow::bail!(
                    "bdmt parse error at offset {}: {e}",
                    reader.buffer_position()
                );
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let local = strip_prefix(&e.name().0).to_string();
                if local == "title"
                    && path
                        .iter()
                        .any(|s| s == "tableofcontents" || s == "titles")
                {
                    if let Some(num) = e
                        .attributes()
                        .with_checks(false)
                        .filter_map(|a| a.ok())
                        .find(|a| strip_prefix(a.key.as_ref()) == "titleNumber")
                        .and_then(|a| {
                            String::from_utf8(a.value.to_vec())
                                .ok()
                                .and_then(|s| s.parse::<u32>().ok())
                        })
                    {
                        current_toc_title_number = Some(num);
                    }
                }
                path.push(local);
            }
            Ok(Event::Empty(_)) => {
                // self-closing element; no text inside
            }
            Ok(Event::End(_)) => {
                if let Some(closed) = path.pop() {
                    if closed == "title"
                        && path
                            .iter()
                            .any(|s| s == "tableofcontents" || s == "titles")
                    {
                        current_toc_title_number = None;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let text = t
                    .unescape()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| {
                        String::from_utf8_lossy(t.as_ref()).into_owned()
                    });
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let last = path.last().map(|s| s.as_str()).unwrap_or("");
                let in_discinfo = path.iter().any(|s| s == "discinfo");
                let in_toc = path.iter().any(|s| s == "tableofcontents");
                match last {
                    "name" if in_discinfo && out.disc_title.is_none() => {
                        out.disc_title = Some(text.to_string());
                    }
                    "numSets" if in_discinfo => {
                        out.num_sets = text.parse().ok();
                    }
                    "setNumber" if in_discinfo => {
                        out.set_number = text.parse().ok();
                    }
                    "titleName" if in_toc => {
                        if let Some(n) = current_toc_title_number {
                            out.title_names.insert(n, text.to_string());
                        }
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
        }
        buf.clear();
    }

    Ok(out)
}

fn strip_prefix(qname: &[u8]) -> &str {
    let s = std::str::from_utf8(qname).unwrap_or("");
    match s.rfind(':') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_disclib_with_disc_title() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<disclib xmlns="urn:BDA:bdmv;disclib">
  <di:discinfo xmlns:di="urn:BDA:bdmv;discinfo">
    <di:title>
      <di:name>Skyfall</di:name>
    </di:title>
  </di:discinfo>
</disclib>"#;
        let md = parse(xml).unwrap();
        assert_eq!(md.disc_title.as_deref(), Some("Skyfall"));
        assert_eq!(md.num_sets, None);
        assert_eq!(md.set_number, None);
        assert!(md.title_names.is_empty());
    }

    #[test]
    fn parses_multi_disc_set_with_set_number() {
        let xml = r#"<disclib xmlns="urn:BDA:bdmv;disclib">
  <di:discinfo xmlns:di="urn:BDA:bdmv;discinfo">
    <di:title>
      <di:name>The Lord of the Rings: The Fellowship of the Ring</di:name>
      <di:numSets>4</di:numSets>
      <di:setNumber>2</di:setNumber>
    </di:title>
  </di:discinfo>
</disclib>"#;
        let md = parse(xml).unwrap();
        assert_eq!(
            md.disc_title.as_deref(),
            Some("The Lord of the Rings: The Fellowship of the Ring")
        );
        assert_eq!(md.num_sets, Some(4));
        assert_eq!(md.set_number, Some(2));
    }

    #[test]
    fn parses_table_of_contents_per_title_names() {
        let xml = r#"<disclib xmlns="urn:BDA:bdmv;disclib">
  <di:discinfo xmlns:di="urn:BDA:bdmv;discinfo">
    <di:title>
      <di:name>Some Movie</di:name>
    </di:title>
  </di:discinfo>
  <di:tableofcontents xmlns:di="urn:BDA:bdmv;tableofcontents">
    <di:titles>
      <di:title titleNumber="1">
        <di:titleName>Main Movie</di:titleName>
      </di:title>
      <di:title titleNumber="2">
        <di:titleName>Theatrical Trailer</di:titleName>
      </di:title>
    </di:titles>
  </di:tableofcontents>
</disclib>"#;
        let md = parse(xml).unwrap();
        assert_eq!(md.disc_title.as_deref(), Some("Some Movie"));
        assert_eq!(md.title_names.len(), 2);
        assert_eq!(md.title_names.get(&1).map(|s| s.as_str()), Some("Main Movie"));
        assert_eq!(
            md.title_names.get(&2).map(|s| s.as_str()),
            Some("Theatrical Trailer")
        );
    }

    #[test]
    fn entity_escaped_titles_round_trip() {
        let xml = r#"<disclib xmlns="urn:BDA:bdmv;disclib">
  <di:discinfo xmlns:di="urn:BDA:bdmv;discinfo">
    <di:title>
      <di:name>Beauty &amp; the Beast</di:name>
    </di:title>
  </di:discinfo>
</disclib>"#;
        let md = parse(xml).unwrap();
        assert_eq!(md.disc_title.as_deref(), Some("Beauty & the Beast"));
    }

    #[test]
    fn empty_meta_returns_empty_metadata() {
        let xml = r#"<disclib xmlns="urn:BDA:bdmv;disclib"></disclib>"#;
        let md = parse(xml).unwrap();
        assert!(md.is_empty());
    }

    #[test]
    fn read_from_mount_returns_none_when_meta_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let res = read_from_mount(tmp.path()).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn read_from_mount_reads_bdmt_eng_xml() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("BDMV/META/DL");
        std::fs::create_dir_all(&dir).unwrap();
        let xml = r#"<disclib xmlns="urn:BDA:bdmv;disclib">
<di:discinfo xmlns:di="urn:BDA:bdmv;discinfo">
<di:title><di:name>Test Title</di:name></di:title>
</di:discinfo>
</disclib>"#;
        std::fs::write(dir.join("bdmt_eng.xml"), xml).unwrap();
        let md = read_from_mount(tmp.path()).unwrap().unwrap();
        assert_eq!(md.disc_title.as_deref(), Some("Test Title"));
    }

    #[test]
    fn read_from_mount_falls_back_to_non_english_when_english_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("BDMV/META/DL");
        std::fs::create_dir_all(&dir).unwrap();
        let xml = r#"<disclib xmlns="urn:BDA:bdmv;disclib">
<di:discinfo xmlns:di="urn:BDA:bdmv;discinfo">
<di:title><di:name>Тестовый диск</di:name></di:title>
</di:discinfo>
</disclib>"#;
        std::fs::write(dir.join("bdmt_rus.xml"), xml).unwrap();
        let md = read_from_mount(tmp.path()).unwrap().unwrap();
        assert_eq!(md.disc_title.as_deref(), Some("Тестовый диск"));
    }
}
