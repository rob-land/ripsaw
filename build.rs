// Build script:
//   1. Compile Blueprint (.blp) UI definitions into GTK Builder XML (.ui).
//   2. Bundle the .ui files into a binary .gresource.
//
// The .gresource lands in $OUT_DIR and is embedded into the final binary via
// `gio::resources_register_include!("ripsaw.gresource")` in main.rs.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const BLUEPRINTS: &[&str] = &[
    "data/resources/ui/window.blp",
    "data/resources/ui/title-list-page.blp",
    "data/resources/ui/title-detail-page.blp",
    "data/resources/ui/rip-progress-page.blp",
    "data/resources/ui/preferences-dialog.blp",
    "data/resources/ui/help-overlay.blp",
];

// Icon sizes to ship in the binary's gresource. GTK auto-registers
// `<resource_base_path>/icons` as a resource path on the default
// IconTheme, so files at `icons/<size>/apps/land.rob.ripsaw.png`
// inside the gresource become findable by the icon-name
// "land.rob.ripsaw" -- which is what AboutDialog / NavigationPage /
// Window header all use. 64/128/256/512 cover the sizes GNOME shells
// actually request.
const ICON_SIZES: &[u32] = &[64, 128, 256, 512];

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

    // Copy each app icon size into OUT_DIR/icons/<size>x<size>/apps so the
    // gresource compiler can pick them up alongside the .ui files. We don't
    // point compile_resources at the source tree directly because the .ui
    // paths it already uses are OUT_DIR-relative.
    for &size in ICON_SIZES {
        let src = manifest_dir
            .join("data/icons/hicolor")
            .join(format!("{size}x{size}"))
            .join("apps")
            .join("land.rob.ripsaw.png");
        let dst_dir = out_dir
            .join("icons")
            .join(format!("{size}x{size}"))
            .join("apps");
        fs::create_dir_all(&dst_dir).expect("create icon out dir");
        let dst = dst_dir.join("land.rob.ripsaw.png");
        fs::copy(&src, &dst).unwrap_or_else(|e| {
            panic!(
                "copy icon {} -> {}: {e}",
                src.display(),
                dst.display()
            )
        });
        println!("cargo:rerun-if-changed={}", src.display());
    }

    // Generate a gresource.xml whose <file> entries reference the .ui files
    // we just compiled into OUT_DIR. We don't reuse data/resources/resources.gresource.xml
    // because its paths are source-relative and we need OUT_DIR-relative paths here.
    let gresource_xml_path = out_dir.join("resources.gresource.xml");
    let ui_files: String = BLUEPRINTS
        .iter()
        .map(|blp| {
            let stem = PathBuf::from(blp)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            // GtkApplicationWindow looks for help-overlay.ui under
            // <resource_base_path>/gtk/, so the help overlay must land at
            // a gtk/-prefixed alias inside the bundle while still being
            // sourced from the same ui/ tree.
            if stem == "help-overlay" {
                format!(
                    r#"    <file alias="gtk/help-overlay.ui" compressed="true" preprocess="xml-stripblanks">ui/{stem}.ui</file>"#
                )
            } else {
                format!(
                    r#"    <file compressed="true" preprocess="xml-stripblanks">ui/{stem}.ui</file>"#
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let icon_files: String = ICON_SIZES
        .iter()
        .map(|&size| {
            // PNGs are already compressed; don't waste cycles re-compressing.
            format!(
                r#"    <file>icons/{size}x{size}/apps/land.rob.ripsaw.png</file>"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<gresources>
  <gresource prefix="/land/rob/ripsaw">
{ui_files}
{icon_files}
  </gresource>
</gresources>
"#
    );
    fs::write(&gresource_xml_path, xml).expect("write gresource.xml");

    glib_build_tools::compile_resources(
        &[&out_dir],
        gresource_xml_path.to_str().expect("utf-8 path"),
        "ripsaw.gresource",
    );

    println!("cargo:rerun-if-changed=build.rs");
}
