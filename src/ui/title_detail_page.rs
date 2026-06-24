// Per-title edit page. Pushed onto the NavigationView when the user
// clicks the edit affordance on a row in TitleListPage. Captures
// overrides for display title, role, chapter names; persisted back
// to the parent TitleListPage via a `connect_saved` closure so the
// list can drive its TheDiscDB submission flow with the edits.

use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self};
use gtk::CompositeTemplate;

use crate::convert::format::OutputFormat;
use crate::identify::TitleRole;

#[derive(Debug, Default, Clone)]
pub struct TitleEdit {
    pub title_index: u32,
    /// `None` means "use whatever the source / TheDiscDB says".
    pub display_title: Option<String>,
    pub role: Option<TitleRole>,
    /// Per-chapter titles, ordered by 1-based chapter index. Empty
    /// vec means "no overrides; use what's already there".
    pub chapter_titles: Vec<String>,
    /// 3D output format for this title (only meaningful for titles with
    /// an MVC track). `None` = no 3D conversion — rip keeps the raw MVC
    /// track. Chosen on this detail page; consumed by the rip planner.
    pub format: Option<OutputFormat>,
}

/// Map the format ComboRow selection (rows mirror the StringList in
/// title-detail-page.blp) to an `OutputFormat`. Index 0 ("None") → `None`.
fn format_from_index(idx: u32) -> Option<OutputFormat> {
    match idx {
        1 => Some(OutputFormat::FullSbs),
        2 => Some(OutputFormat::HalfSbs),
        3 => Some(OutputFormat::FullTab),
        4 => Some(OutputFormat::HalfTab),
        5 => Some(OutputFormat::FrameSequential),
        _ => None,
    }
}

fn format_to_index(fmt: Option<OutputFormat>) -> u32 {
    match fmt {
        Some(OutputFormat::FullSbs) => 1,
        Some(OutputFormat::HalfSbs) => 2,
        Some(OutputFormat::FullTab) => 3,
        Some(OutputFormat::HalfTab) => 4,
        Some(OutputFormat::FrameSequential) => 5,
        None => 0,
    }
}

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/land/rob/ripsaw/ui/title-detail-page.ui")]
    pub struct TitleDetailPage {
        #[template_child] pub display_title_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub role_row: TemplateChild<adw::ComboRow>,
        #[template_child] pub format_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub format_row: TemplateChild<adw::ComboRow>,
        #[template_child] pub chapters_group: TemplateChild<adw::PreferencesGroup>,
        #[allow(dead_code)]
        #[template_child] pub identity_group: TemplateChild<adw::PreferencesGroup>,
        #[allow(dead_code)]
        #[template_child] pub tracks_group: TemplateChild<adw::PreferencesGroup>,

        pub title_index: RefCell<u32>,
        pub chapter_entries: RefCell<Vec<adw::EntryRow>>,
        /// Callback fires on `hiding`; carries the snapshot of the
        /// fields back to whoever pushed the page.
        pub on_saved: RefCell<Option<Box<dyn Fn(TitleEdit)>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TitleDetailPage {
        const NAME: &'static str = "RipsawTitleDetailPage";
        type Type = super::TitleDetailPage;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for TitleDetailPage {
        fn constructed(&self) {
            self.parent_constructed();
            // When the page is popped (back navigation), gather the
            // current field values and hand them to the saved-callback.
            let weak: glib::WeakRef<super::TitleDetailPage> = self.obj().downgrade();
            self.obj().connect_hiding(move |_| {
                if let Some(page) = weak.upgrade() {
                    if let Some(cb) = page.imp().on_saved.borrow().as_ref() {
                        cb(page.snapshot_edit());
                    }
                }
            });
        }
    }
    impl WidgetImpl for TitleDetailPage {}
    impl NavigationPageImpl for TitleDetailPage {}
}

glib::wrapper! {
    pub struct TitleDetailPage(ObjectSubclass<imp::TitleDetailPage>)
        @extends gtk::Widget, adw::NavigationPage,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for TitleDetailPage {
    fn default() -> Self { glib::Object::new() }
}

impl TitleDetailPage {
    /// Populate the page with the title's current state.
    /// `display_title_default` is what the placeholder shows when the
    /// user hasn't typed an override (TheDiscDB display title, or the
    /// MakeMKV track name as a fallback). `chapter_defaults` are the
    /// chapter titles to seed the editable list (also typically from
    /// TheDiscDB; empty when the disc is unidentified).
    pub fn populate(
        &self,
        edit: &TitleEdit,
        display_title_default: &str,
        chapter_defaults: &[String],
        is_3d: bool,
    ) {
        self.set_title(&format!("Title {} details", edit.title_index));
        self.imp().title_index.replace(edit.title_index);

        // 3D output format: only shown for titles that carry an MVC track
        // (the format group is hidden otherwise). Used to be a per-row
        // dropdown on the title list.
        self.imp().format_group.set_visible(is_3d);
        self.imp().format_row.set_selected(format_to_index(edit.format));

        // Display title: user override wins, then the default.
        let initial = edit
            .display_title
            .clone()
            .unwrap_or_else(|| display_title_default.to_string());
        self.imp().display_title_row.set_text(&initial);

        // Role combo (rows match the StringList order in the .blp).
        let role_index = match edit.role {
            Some(TitleRole::Main) => 0,
            Some(TitleRole::Trailer) => 1,
            Some(TitleRole::BehindTheScenes) => 2,
            Some(TitleRole::DeletedScene) => 3,
            Some(TitleRole::Featurette) => 4,
            Some(TitleRole::Interview) => 5,
            Some(TitleRole::Scene) => 6,
            Some(TitleRole::Short) => 7,
            _ => 8,
        };
        self.imp().role_row.set_selected(role_index);

        // Chapter rows: one EntryRow per chapter, seeded with the
        // override (when present) or the default (when not).
        let group = self.imp().chapters_group.get();
        let mut entries = self.imp().chapter_entries.borrow_mut();
        for row in entries.drain(..) {
            group.remove(&row);
        }
        let count = edit
            .chapter_titles
            .len()
            .max(chapter_defaults.len());
        if count == 0 {
            // No chapters known. Add a placeholder action row so the
            // user doesn't see an empty group.
            let row = adw::ActionRow::builder()
                .title("No chapters known")
                .subtitle("Rip the title first to inspect on-disc chapters, or wait for TheDiscDB to populate them.")
                .sensitive(false)
                .build();
            group.add(&row);
        }
        for i in 0..count {
            let placeholder = chapter_defaults
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("Chapter {}", i + 1));
            let initial = edit
                .chapter_titles
                .get(i)
                .cloned()
                .unwrap_or_else(|| placeholder.clone());
            let row = adw::EntryRow::builder()
                .title(format!("Chapter {}", i + 1))
                .text(&initial)
                .build();
            group.add(&row);
            entries.push(row);
        }
    }

    /// Register a callback that fires when the page is hidden (i.e.,
    /// the user navigates back). Receives the current snapshot of
    /// edits as a `TitleEdit`.
    pub fn connect_saved<F: Fn(TitleEdit) + 'static>(&self, f: F) {
        self.imp().on_saved.replace(Some(Box::new(f)));
    }

    /// Build a `TitleEdit` from the current state of the editable
    /// fields. Empty strings → `None` so the upstream "no override"
    /// semantics are preserved.
    pub fn snapshot_edit(&self) -> TitleEdit {
        let title_index = *self.imp().title_index.borrow();
        let display_title = {
            let raw = self.imp().display_title_row.text().to_string();
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        let role = match self.imp().role_row.selected() {
            0 => Some(TitleRole::Main),
            1 => Some(TitleRole::Trailer),
            2 => Some(TitleRole::BehindTheScenes),
            3 => Some(TitleRole::DeletedScene),
            4 => Some(TitleRole::Featurette),
            5 => Some(TitleRole::Interview),
            6 => Some(TitleRole::Scene),
            7 => Some(TitleRole::Short),
            _ => Some(TitleRole::Other),
        };
        let chapter_titles: Vec<String> = self
            .imp()
            .chapter_entries
            .borrow()
            .iter()
            .map(|r| r.text().trim().to_string())
            .collect();
        let format = format_from_index(self.imp().format_row.selected());
        TitleEdit { title_index, display_title, role, chapter_titles, format }
    }
}
