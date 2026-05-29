use std::cell::RefCell;
use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, clone};
use gtk::{gio, CompositeTemplate};

use crate::identify::composite::{analyze_relations, TitleRelation};
use crate::identify::pipeline::IdentificationResult;
use crate::identify::DiscType;
use crate::rip::makemkv_parse::{MakemkvScan, TitleAttributes};
use crate::rip::plan::{
    auto_detect_content_kind, default_library_root, naming_opts_for_unidentified, plan_rip,
    DiscContentKind,
};
use crate::settings::settings;
use crate::ui::rip_progress_page::{RipProgressPage, RipQueueItem};

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/dev/threedrip/ThreeDrip/ui/title-list-page.ui")]
    pub struct TitleListPage {
        #[template_child] pub title_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub rip_button: TemplateChild<gtk::Button>,
        #[template_child] pub series_toggle: TemplateChild<adw::SwitchRow>,

        pub checkboxes: RefCell<Vec<gtk::CheckButton>>,
        pub titles: RefCell<Vec<TitleAttributes>>,
        pub iso_path: RefCell<Option<PathBuf>>,
        pub disc_name: RefCell<Option<String>>,
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

    impl ObjectImpl for TitleListPage {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_actions();
        }
    }
    impl WidgetImpl for TitleListPage {}
    impl NavigationPageImpl for TitleListPage {}
}

glib::wrapper! {
    pub struct TitleListPage(ObjectSubclass<imp::TitleListPage>)
        @extends gtk::Widget, adw::NavigationPage,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for TitleListPage {
    fn default() -> Self { glib::Object::new() }
}

impl TitleListPage {
    pub fn from_identification(result: &IdentificationResult, iso_path: PathBuf) -> Self {
        let page: Self = glib::Object::new();
        page.imp().iso_path.replace(Some(iso_path));
        page.imp().disc_name.replace(result.scan.disc.name.clone());
        page.imp().titles.replace(result.scan.titles.clone());
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
        let detected = auto_detect_content_kind(&result.scan.titles);
        self.imp().series_toggle.set_active(detected == DiscContentKind::Series);
        self.populate_rows(&result.scan);
    }

    fn populate_rows(&self, scan: &MakemkvScan) {
        let group = self.imp().title_group.get();
        let mut checkboxes = Vec::with_capacity(scan.titles.len());

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

            let check = gtk::CheckButton::new();
            check.set_valign(gtk::Align::Center);
            check.connect_toggled(clone!(
                #[weak(rename_to = page)]
                self,
                move |_| page.refresh_rip_sensitivity()
            ));

            let row = adw::ActionRow::builder()
                .title(title_label)
                .subtitle(subtitle_parts.join("  •  "))
                .activatable(true)
                .build();
            row.add_prefix(&check);
            row.set_activatable_widget(Some(&check));
            group.add(&row);
            checkboxes.push(check);
        }

        self.imp().checkboxes.replace(checkboxes);
        self.refresh_rip_sensitivity();
    }

    fn refresh_rip_sensitivity(&self) {
        let any_checked = self
            .imp()
            .checkboxes
            .borrow()
            .iter()
            .any(|c| c.is_active());
        self.imp().rip_button.set_sensitive(any_checked);
    }

    fn setup_actions(&self) {
        let rip_action = gio::SimpleAction::new("rip-selected", None);
        rip_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.start_rip()
        ));

        let group = gio::SimpleActionGroup::new();
        group.add_action(&rip_action);
        self.insert_action_group("page", Some(&group));
    }

    fn selected_indexes(&self) -> Vec<u32> {
        let titles = self.imp().titles.borrow();
        self.imp()
            .checkboxes
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                if c.is_active() {
                    titles.get(i).map(|t| t.index)
                } else {
                    None
                }
            })
            .collect()
    }

    fn start_rip(&self) {
        let selected = self.selected_indexes();
        if selected.is_empty() {
            return;
        }
        let iso_path = match self.imp().iso_path.borrow().clone() {
            Some(p) => p,
            None => {
                tracing::error!("rip requested but iso_path not set on TitleListPage");
                return;
            }
        };
        let disc_name = self
            .imp()
            .disc_name
            .borrow()
            .clone()
            .unwrap_or_else(|| "Unknown Disc".to_string());

        let titles_snapshot = self.imp().titles.borrow().clone();
        let identification_for_plan = build_pseudo_identification(&titles_snapshot, &disc_name);

        let user_settings = settings().lock().expect("settings mutex").clone();
        let library_root = user_settings
            .library_root
            .clone()
            .unwrap_or_else(default_library_root);
        let scheme = user_settings.scheme;
        let content_kind = if self.imp().series_toggle.is_active() {
            DiscContentKind::Series
        } else {
            DiscContentKind::Movie
        };
        let naming_opts = naming_opts_for_unidentified(
            library_root.clone(),
            scheme,
            content_kind,
            &disc_name,
        );
        let plan = plan_rip(&identification_for_plan, &selected, Some(&naming_opts));
        let queue: Vec<RipQueueItem> = plan.into_iter().map(RipQueueItem::from).collect();

        let progress = RipProgressPage::default();
        progress.set_queue(&queue);
        progress.append_log(&format!(
            "Library root: {} • Scheme: {} • {}",
            library_root.display(),
            scheme.label(),
            match content_kind {
                DiscContentKind::Movie => "Treating as movie",
                DiscContentKind::Series => "Treating as series",
            },
        ));

        if let Some(nav) = navigation_view(self) {
            nav.push(&progress);
        } else {
            tracing::warn!("TitleListPage has no NavigationView ancestor; cannot push RipProgressPage");
        }

        crate::rip::orchestrator::run_rip_queue(iso_path, queue, progress.downgrade());
    }
}

fn build_pseudo_identification(
    titles: &[TitleAttributes],
    disc_name: &str,
) -> IdentificationResult {
    IdentificationResult {
        scan: MakemkvScan {
            disc: crate::rip::makemkv_parse::DiscAttributes {
                name: Some(disc_name.to_string()),
                ..Default::default()
            },
            titles: titles.to_vec(),
            ..Default::default()
        },
        mount: None,
        disc_type: DiscType::BluRay,
        content_hash: None,
        identities: Vec::new(),
    }
}

fn navigation_view(page: &TitleListPage) -> Option<adw::NavigationView> {
    let mut next: Option<gtk::Widget> = page.parent();
    while let Some(widget) = next {
        if let Ok(nav) = widget.clone().downcast::<adw::NavigationView>() {
            return Some(nav);
        }
        next = widget.parent();
    }
    None
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
