use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::CompositeTemplate;

use crate::identify::composite::{analyze_relations, TitleRelation};
use crate::identify::pipeline::IdentificationResult;
use crate::identify::DiscType;
use crate::rip::makemkv_parse::MakemkvScan;

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/dev/threedrip/ThreeDrip/ui/title-list-page.ui")]
    pub struct TitleListPage {
        #[template_child]
        pub title_group: TemplateChild<adw::PreferencesGroup>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TitleListPage {
        const NAME: &'static str = "ThreeDripTitleListPage";
        type Type = super::TitleListPage;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for TitleListPage {}
    impl WidgetImpl for TitleListPage {}
    impl NavigationPageImpl for TitleListPage {}
}

glib::wrapper! {
    pub struct TitleListPage(ObjectSubclass<imp::TitleListPage>)
        @extends gtk::Widget, adw::NavigationPage,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for TitleListPage {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl TitleListPage {
    pub fn from_scan(scan: &MakemkvScan) -> Self {
        let page: Self = glib::Object::new();
        page.populate(scan);
        page
    }

    pub fn from_identification(result: &IdentificationResult) -> Self {
        let page: Self = glib::Object::new();
        page.populate_with_identity(result);
        page
    }

    fn populate_with_identity(&self, result: &IdentificationResult) {
        let group = self.imp().title_group.get();
        group.set_title(&format_group_title(result));
        group.set_description(Some(&format_group_description(result)));
        if let Some(name) = &result.scan.disc.name {
            self.set_title(name);
        }
        self.populate_rows(&result.scan);
    }

    pub fn populate(&self, scan: &MakemkvScan) {
        let group = self.imp().title_group.get();

        if let Some(name) = &scan.disc.name {
            self.set_title(name);
        }
        let _ = group;

        self.populate_rows(scan);
    }

    fn populate_rows(&self, scan: &MakemkvScan) {
        let group = self.imp().title_group.get();
        let pairs: Vec<(u32, &str)> = scan
            .titles
            .iter()
            .map(|t| (t.index, t.segment_map.as_deref().unwrap_or("")))
            .collect();
        let relations = analyze_relations(&pairs);

        for (t, relation) in scan.titles.iter().zip(relations.iter()) {
            let title_label = match (t.index, t.name.as_deref()) {
                (idx, Some(n)) if !n.is_empty() => format!("Title {idx} — {n}"),
                (idx, _) => format!("Title {idx}"),
            };
            let duration = format_duration(t.duration_seconds.unwrap_or(0));
            let size = format_bytes(t.size_bytes.unwrap_or(0));
            let source = t.source_file.as_deref().unwrap_or("?");

            let mut subtitle_parts = vec![duration, size, source.to_string()];
            match relation {
                TitleRelation::Composite { constituents } => {
                    subtitle_parts.push(format!("contains {} other title(s)", constituents.len()));
                }
                TitleRelation::Constituent { containers } => {
                    let parents: Vec<String> = containers.iter().map(|i| format!("#{i}")).collect();
                    subtitle_parts.push(format!("part of {}", parents.join(", ")));
                }
                TitleRelation::Atomic => {}
            }

            let row = adw::ActionRow::builder()
                .title(title_label)
                .subtitle(subtitle_parts.join("  •  "))
                .build();
            group.add(&row);
        }
    }
}

fn format_group_title(result: &IdentificationResult) -> String {
    if let Some(first) = result.identities.first() {
        if result.identities.len() == 1 {
            format!("Identified as {}", first.release_slug)
        } else {
            format!("{} matching releases", result.identities.len())
        }
    } else {
        "Not in TheDiscDB catalog".into()
    }
}

fn format_group_description(result: &IdentificationResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("{} detected", format_disc_type(result.disc_type)));
    if let Some(h) = &result.content_hash {
        parts.push(format!("content hash {h}"));
    } else if result.mount.is_some() {
        parts.push("content hash unavailable".into());
    } else {
        parts.push("could not mount for hashing".into());
    }
    if !result.is_identified() {
        parts.push("submit a contribution to extend the catalog".into());
    }
    parts.join("  •  ")
}

fn format_disc_type(t: DiscType) -> &'static str {
    match t {
        DiscType::Dvd => "DVD",
        DiscType::BluRay => "Blu-ray",
        DiscType::UltraHdBluRay => "4K UHD Blu-ray",
        DiscType::BluRay3D => "3D Blu-ray",
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_duration_handles_short_and_long() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(60), "1:00");
        assert_eq!(format_duration(3599), "59:59");
        assert_eq!(format_duration(3600), "1:00:00");
        assert_eq!(format_duration(2 * 3600 + 6 * 60 + 42), "2:06:42");
    }

    #[test]
    fn formats_bytes_with_decimal_threshold() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500_000), "2 MB");
        assert_eq!(format_bytes(2_400_000_000), "2.4 GB");
        assert_eq!(format_bytes(43_274_268_672), "43.3 GB");
    }
}
