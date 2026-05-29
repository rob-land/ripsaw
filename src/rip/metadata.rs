// Post-rip MKV metadata application.
//
// After makemkvcon produces an MKV and the orchestrator renames it
// to the library path, this module patches in two pieces of
// metadata using `mkvpropedit`:
//
//   - **Chapter titles**. MakeMKV writes the chapter timestamps that
//     came off the disc but leaves the chapter names blank. TheDiscDB
//     supplies the names per chapter index (1-based). We dump the
//     existing chapters via `mkvextract chapters`, splice in the
//     titles at matching indices, and apply with `mkvpropedit -c`.
//   - **Segment title**. The Matroska Segment.Title element is what
//     some players show as the "name of the video". Our convention
//     (per user preference) is to set it equal to the filename
//     basename so the on-screen name always matches the file on disk.
//
// Both operations are best-effort: if mkvpropedit/mkvextract are
// missing, or if the chapter XML doesn't line up, we log a warning
// and leave the file alone -- the rip itself isn't blocked.

use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;

/// Apply chapter titles + segment title to `path`. Returns `Ok(())`
/// whether or not changes were made; on individual failures, logs a
/// warning via `tracing` and continues. The caller treats a returned
/// error as fatal, so we only propagate "I/O setup failed" errors --
/// missing external tools or chapter-count mismatches are non-fatal.
pub async fn apply_post_rip_metadata(
    path: &Path,
    chapter_titles: &[String],
    segment_title: Option<&str>,
) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!(
            "post-rip metadata: file {} does not exist",
            path.display()
        ));
    }

    if !chapter_titles.is_empty() {
        if let Err(e) = apply_chapter_titles(path, chapter_titles).await {
            tracing::warn!(
                "post-rip chapter titles for {} skipped: {e:#}",
                path.display()
            );
        }
    }

    if let Some(title) = segment_title.filter(|s| !s.is_empty()) {
        if let Err(e) = apply_segment_title(path, title).await {
            tracing::warn!(
                "post-rip segment title for {} skipped: {e:#}",
                path.display()
            );
        }
    }

    Ok(())
}

async fn apply_chapter_titles(path: &Path, titles: &[String]) -> Result<()> {
    // 1. Dump existing chapters from the MKV -- this preserves the
    //    ChapterTimeStart values MakeMKV wrote.
    let extracted = Command::new("mkvextract")
        .arg(path)
        .arg("chapters")
        .arg("--")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running mkvextract chapters")?;
    if !extracted.status.success() {
        return Err(anyhow!(
            "mkvextract chapters failed: {}",
            String::from_utf8_lossy(&extracted.stderr).trim()
        ));
    }
    let xml = String::from_utf8(extracted.stdout)
        .context("mkvextract chapters returned non-UTF-8")?;
    if xml.trim().is_empty() {
        return Err(anyhow!("MKV has no chapter atoms to label"));
    }

    let updated = splice_chapter_titles(&xml, titles)?;

    // 2. Write to a temp file next to the MKV and feed it to mkvpropedit.
    let tmp = path.with_extension("chapters.xml.tmp");
    tokio::fs::write(&tmp, &updated)
        .await
        .context("writing temporary chapters.xml")?;

    let status = Command::new("mkvpropedit")
        .arg(path)
        .arg("-c")
        .arg(&tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .context("running mkvpropedit -c")?;
    let _ = tokio::fs::remove_file(&tmp).await;

    if !status.success() {
        return Err(anyhow!("mkvpropedit -c exited with {}", status));
    }
    Ok(())
}

async fn apply_segment_title(path: &Path, title: &str) -> Result<()> {
    let status = Command::new("mkvpropedit")
        .arg(path)
        .arg("--edit")
        .arg("info")
        .arg("--set")
        .arg(format!("title={title}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .context("running mkvpropedit --set title")?;
    if !status.success() {
        return Err(anyhow!("mkvpropedit --set title exited with {}", status));
    }
    Ok(())
}

/// Given a Matroska chapters XML (the output of `mkvextract chapters`)
/// and a list of chapter titles ordered by index starting at 1, return
/// a new XML where each `<ChapterAtom>` gets its `<ChapterDisplay>` /
/// `<ChapterString>` replaced (or inserted) with the matching title.
///
/// Pure function, public for unit testing.
pub fn splice_chapter_titles(xml: &str, titles: &[String]) -> Result<String> {
    // Count the existing <ChapterAtom> markers. Mismatches (TheDiscDB
    // disagreeing with the disc) are surfaced as an error so the
    // caller can log + skip rather than corrupt the file.
    let atom_count = xml.match_indices("<ChapterAtom>").count();
    if atom_count == 0 {
        return Err(anyhow!("chapters XML has no <ChapterAtom> elements"));
    }
    if atom_count != titles.len() {
        return Err(anyhow!(
            "chapter count mismatch: disc has {atom_count}, TheDiscDB has {}",
            titles.len()
        ));
    }

    let mut out = String::with_capacity(xml.len() + 128 * titles.len());
    let mut cursor = 0usize;
    let mut titles_iter = titles.iter();
    while let Some(rel) = xml[cursor..].find("<ChapterAtom>") {
        let atom_start = cursor + rel;
        let atom_close_rel = xml[atom_start..]
            .find("</ChapterAtom>")
            .ok_or_else(|| anyhow!("unterminated <ChapterAtom>"))?;
        let atom_close = atom_start + atom_close_rel;
        let atom = &xml[atom_start..atom_close];

        out.push_str(&xml[cursor..atom_start]);

        let title = titles_iter.next().expect("count was validated above");
        let rewritten = rewrite_atom_with_title(atom, title);
        out.push_str(&rewritten);

        cursor = atom_close;
    }
    out.push_str(&xml[cursor..]);
    Ok(out)
}

fn rewrite_atom_with_title(atom: &str, title: &str) -> String {
    let escaped = escape_xml_text(title);
    // If a <ChapterDisplay> already exists, replace its first
    // <ChapterString> body. Otherwise insert a fresh ChapterDisplay
    // block just before </ChapterAtom> (we never see </ChapterAtom>
    // here -- the caller splits on it).
    if let Some(disp_idx) = atom.find("<ChapterDisplay>") {
        let disp_close_idx = atom[disp_idx..]
            .find("</ChapterDisplay>")
            .map(|p| disp_idx + p);
        if let Some(disp_close) = disp_close_idx {
            let display_block = &atom[disp_idx..disp_close + "</ChapterDisplay>".len()];
            let rewritten_display = if display_block.contains("<ChapterString>") {
                replace_first_element_body(display_block, "ChapterString", &escaped)
            } else {
                insert_string_into_display(display_block, &escaped)
            };
            let mut out = String::with_capacity(atom.len() + escaped.len() + 32);
            out.push_str(&atom[..disp_idx]);
            out.push_str(&rewritten_display);
            out.push_str(&atom[disp_close + "</ChapterDisplay>".len()..]);
            return out;
        }
    }
    // No ChapterDisplay -- append one before the trailing whitespace
    // (the caller passes the atom *body*, not including </ChapterAtom>).
    let display = format!(
        "<ChapterDisplay><ChapterString>{escaped}</ChapterString>\
        <ChapterLanguage>eng</ChapterLanguage></ChapterDisplay>"
    );
    let mut out = String::with_capacity(atom.len() + display.len());
    out.push_str(atom.trim_end());
    out.push_str(&display);
    out
}

fn replace_first_element_body(block: &str, tag: &str, new_body: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if let Some(open_idx) = block.find(&open) {
        if let Some(close_rel) = block[open_idx..].find(&close) {
            let body_start = open_idx + open.len();
            let body_end = open_idx + close_rel;
            let mut out = String::with_capacity(block.len() + new_body.len());
            out.push_str(&block[..body_start]);
            out.push_str(new_body);
            out.push_str(&block[body_end..]);
            return out;
        }
    }
    block.to_string()
}

fn insert_string_into_display(display_block: &str, escaped_title: &str) -> String {
    // Insert <ChapterString>title</ChapterString> right after
    // <ChapterDisplay>.
    let after_open = "<ChapterDisplay>";
    if let Some(p) = display_block.find(after_open) {
        let insert_at = p + after_open.len();
        let mut out = String::with_capacity(display_block.len() + escaped_title.len() + 32);
        out.push_str(&display_block[..insert_at]);
        out.push_str("<ChapterString>");
        out.push_str(escaped_title);
        out.push_str("</ChapterString>");
        out.push_str(&display_block[insert_at..]);
        return out;
    }
    display_block.to_string()
}

fn escape_xml_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<Chapters>
  <EditionEntry>
    <ChapterAtom>
      <ChapterTimeStart>00:00:00.000000000</ChapterTimeStart>
      <ChapterDisplay>
        <ChapterString></ChapterString>
        <ChapterLanguage>eng</ChapterLanguage>
      </ChapterDisplay>
    </ChapterAtom>
    <ChapterAtom>
      <ChapterTimeStart>00:01:30.000000000</ChapterTimeStart>
      <ChapterDisplay>
        <ChapterString>Chapter 02</ChapterString>
        <ChapterLanguage>eng</ChapterLanguage>
      </ChapterDisplay>
    </ChapterAtom>
  </EditionEntry>
</Chapters>"#;

    #[test]
    fn splice_replaces_chapter_strings_in_order() {
        let titles = vec!["Agent Down".to_string(), "The Chase".to_string()];
        let out = splice_chapter_titles(SAMPLE, &titles).unwrap();
        assert!(
            out.contains("<ChapterString>Agent Down</ChapterString>"),
            "first title missing: {out}"
        );
        assert!(
            out.contains("<ChapterString>The Chase</ChapterString>"),
            "second title missing: {out}"
        );
        // Timestamps preserved.
        assert!(out.contains("00:00:00.000000000"));
        assert!(out.contains("00:01:30.000000000"));
    }

    #[test]
    fn splice_rejects_count_mismatch() {
        // 2 atoms, 1 title.
        let err = splice_chapter_titles(SAMPLE, &["only".to_string()]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("chapter count mismatch"), "got: {msg}");
    }

    #[test]
    fn splice_escapes_xml_special_chars() {
        let xml = r#"<Chapters>
  <ChapterAtom>
    <ChapterTimeStart>00:00:00.000000000</ChapterTimeStart>
    <ChapterDisplay><ChapterString></ChapterString></ChapterDisplay>
  </ChapterAtom>
</Chapters>"#;
        let titles = vec!["Q & R <test>".to_string()];
        let out = splice_chapter_titles(xml, &titles).unwrap();
        assert!(out.contains("Q &amp; R &lt;test&gt;"), "got: {out}");
        // Make sure we didn't leave the raw chars in.
        assert!(!out.contains("Q & R <test>"), "got: {out}");
    }

    #[test]
    fn splice_inserts_display_when_atom_has_only_timestamp() {
        let xml = r#"<Chapters>
  <ChapterAtom>
    <ChapterTimeStart>00:00:00.000000000</ChapterTimeStart>
  </ChapterAtom>
</Chapters>"#;
        let titles = vec!["Intro".to_string()];
        let out = splice_chapter_titles(xml, &titles).unwrap();
        assert!(out.contains("<ChapterString>Intro</ChapterString>"), "got: {out}");
        assert!(out.contains("<ChapterDisplay>"), "got: {out}");
    }
}
