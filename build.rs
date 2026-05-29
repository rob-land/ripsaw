// Build script:
//   1. Compile Blueprint (.blp) UI definitions into GTK Builder XML (.ui).
//   2. Bundle the .ui files into a binary .gresource.
//
// The .gresource lands in $OUT_DIR and is embedded into the final binary via
// `gio::resources_register_include!("threedrip.gresource")` in main.rs.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const BLUEPRINTS: &[&str] = &[
    "data/resources/ui/window.blp",
    "data/resources/ui/title-list-page.blp",
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let ui_out = out_dir.join("ui");
    fs::create_dir_all(&ui_out).expect("create ui out dir");

    for blp in BLUEPRINTS {
        let blp_path = manifest_dir.join(blp);
        let ui_name = blp_path
            .file_stem()
            .expect("blp filename")
            .to_string_lossy()
            .into_owned()
            + ".ui";
        let ui_path = ui_out.join(&ui_name);

        let status = Command::new("blueprint-compiler")
            .arg("compile")
            .arg("--output")
            .arg(&ui_path)
            .arg(&blp_path)
            .status()
            .expect("blueprint-compiler must be on PATH (install gnome-blueprint-compiler)");
        assert!(status.success(), "blueprint-compiler failed on {}", blp_path.display());

        println!("cargo:rerun-if-changed={}", blp_path.display());
    }

    // Generate a gresource.xml whose <file> entries reference the .ui files
    // we just compiled into OUT_DIR. We don't reuse data/resources/resources.gresource.xml
    // because its paths are source-relative and we need OUT_DIR-relative paths here.
    let gresource_xml_path = out_dir.join("resources.gresource.xml");
    let files: String = BLUEPRINTS
        .iter()
        .map(|blp| {
            let stem = PathBuf::from(blp)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            format!(r#"    <file compressed="true" preprocess="xml-stripblanks">ui/{stem}.ui</file>"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<gresources>
  <gresource prefix="/dev/threedrip/ThreeDrip">
{files}
  </gresource>
</gresources>
"#
    );
    fs::write(&gresource_xml_path, xml).expect("write gresource.xml");

    glib_build_tools::compile_resources(
        &[&out_dir],
        gresource_xml_path.to_str().expect("utf-8 path"),
        "threedrip.gresource",
    );

    println!("cargo:rerun-if-changed=build.rs");
}
