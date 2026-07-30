use std::cell::RefCell;
use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, clone};
use gtk::{gio, CompositeTemplate};

use crate::convert::format::OutputFormat;
use crate::convert::plan::{ConversionPlan, StereoSource};
use crate::convert::runner::run_conversion;
use crate::identify::composite::{analyze_relations, TitleRelation};
use crate::identify::pipeline::IdentificationResult;
use crate::identify::{DiscType, Identity, TitleIdentity, TitleRole};
use crate::integrations::{radarr::RadarrClient, sonarr::SonarrClient};
use crate::rip::makemkv_parse::{MakemkvScan, TitleAttributes};
use crate::rip::plan::{parse_series_label, DiscContentKind};
use std::collections::HashMap;

use crate::rip::plan::{
    auto_detect_content_kind, default_library_root, naming_opts_for_unidentified, plan_rip,
};
use crate::settings::settings;
use crate::ui::rip_progress_page::{RipProgressPage, RipQueueItem};

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/land/rob/ripsaw/ui/title-list-page.ui")]
    pub struct TitleListPage {
        #[template_child] pub title_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub process_button: TemplateChild<gtk::Button>,
        #[template_child] pub submit_button: TemplateChild<gtk::Button>,
        #[template_child] pub series_toggle: TemplateChild<adw::SwitchRow>,
        #[template_child] pub title_override: TemplateChild<adw::EntryRow>,
        #[template_child] pub season_override: TemplateChild<adw::SpinRow>,
        #[template_child] pub year_row: TemplateChild<adw::SpinRow>,
        #[template_child] pub tmdb_id_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub imdb_id_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub plot_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub tagline_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub release_slug_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub release_title_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub locale_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub region_code_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub upc_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub asin_row: TemplateChild<adw::EntryRow>,
        #[allow(dead_code)]
        #[template_child] pub submission_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub submission_expander: TemplateChild<adw::ExpanderRow>,
        #[template_child] pub status_banner: TemplateChild<adw::Banner>,
        #[template_child] pub identity_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub poster_image: TemplateChild<gtk::Picture>,
        #[template_child] pub identity_title: TemplateChild<gtk::Label>,
        #[template_child] pub identity_subtitle: TemplateChild<gtk::Label>,

        pub checkboxes: RefCell<Vec<gtk::CheckButton>>,
        pub episode_entries: RefCell<Vec<gtk::Entry>>,
        pub titles: RefCell<Vec<TitleAttributes>>,
        pub iso_path: RefCell<Option<PathBuf>>,
        pub source: RefCell<Option<crate::rip::makemkv::ScanSource>>,
        pub disc_name: RefCell<Option<String>>,
        pub source_kind: RefCell<Option<StereoSource>>,
        pub identities: RefCell<Vec<Identity>>,
        /// Per-title edits captured on the TitleDetailPage. Keyed by
        /// MakeMKV title index. Empty until the user opens a detail
        /// page and changes something.
        pub title_edits: RefCell<HashMap<u32, crate::ui::title_detail_page::TitleEdit>>,
        /// TheDiscDB content hash for this disc. Empty when the disc
        /// is unidentified or the input wasn't a physical-disc / ISO
        /// source (e.g. a stand-alone MKV).
        pub content_hash: RefCell<String>,
        /// Parsed `BDMV/META/DL/bdmt_eng.xml` content when present.
        /// Pre-fills disc title + per-title display titles when
        /// TheDiscDB has no match.
        pub bdmt: RefCell<Option<crate::identify::bdmt::BdmtMetadata>>,
        /// Full-resolution bytes of the cover image the user picked
        /// via the cover-art picker. `Some` means submit_corrections
        /// should ship this as `cover.jpg` next to metadata.json.
        pub picked_cover_jpg: RefCell<Option<Vec<u8>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TitleListPage {
        const NAME: &'static str = "RipsawTitleListPage";
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

/// Quick-select modes for the title-group header buttons.
#[derive(Clone, Copy)]
enum SelectMode {
    Main,
    All,
    None,
}

impl Default for TitleListPage {
    fn default() -> Self { glib::Object::new() }
}

impl TitleListPage {
    pub fn from_identification(result: &IdentificationResult, iso_path: PathBuf) -> Self {
        let page: Self = glib::Object::new();
        page.imp().iso_path.replace(Some(iso_path));
        page.imp().source.replace(Some(result.source.clone()));
        page.imp().disc_name.replace(result.scan.disc.name.clone());
        page.imp().titles.replace(result.scan.titles.clone());
        // Distinguish mvcC BlockAddition packaging (modern MakeMKV;
        // BlockAddition extractor not yet built) from inline stereo
        // mode 13/14 packaging (which the ldecod pipeline handles
        // end-to-end today).
        let source_kind = if result.has_mvc {
            let has_mvcc_bytes = result
                .source_file
                .as_ref()
                .and_then(|p| std::fs::File::open(p).ok())
                .map(|f| {
                    let mut r = libmvc::ebml::EbmlReader::new(f);
                    matches!(libmvc::mvcc::find_mvcc_bytes(&mut r), Ok(Some(_)))
                })
                .unwrap_or(false);
            if has_mvcc_bytes {
                Some(StereoSource::MvcWithBlockAdditions)
            } else {
                Some(StereoSource::MvcInlineLaced)
            }
        } else {
            None
        };
        page.imp().source_kind.replace(source_kind);
        page.populate_with_identity(result);
        // The 3D output format is now chosen per title (a dropdown on each
        // MVC title row, added in populate_rows); the encoder backend is a
        // global preference. Nothing disc-level to initialise here.
        // 3D-on-old-MakeMKV warning: probe makemkvcon's version and
        // warn (toast) if it's below the version that reliably writes
        // mvcC BlockAddition output. Without that, our keep-mvc
        // profile is silently ignored and the produced MKV is 2D.
        // Diagnosed against Jurassic Park 3D on v1.17.8 (drops MVC)
        // vs the working samples authored by v1.18.2 (writes mvcC).
        if result.has_mvc {
            page.warn_if_makemkv_too_old_for_3d();
        }
        page
    }

    /// Probe makemkvcon's version asynchronously and toast a warning
    /// when it's below the mvcC-capable minimum. Best-effort: silent
    /// when probe fails (no-op if makemkvcon is missing -- the rip
    /// will surface a clearer error in that case).
    fn warn_if_makemkv_too_old_for_3d(&self) {
        let (tx, rx) =
            async_channel::bounded::<crate::rip::makemkv::ProbeOutcome>(1);
        crate::runtime::tokio_runtime().spawn(async move {
            let _ = tx.send(crate::rip::makemkv::probe().await).await;
        });
        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = page)]
            self,
            async move {
                let Ok(outcome) = rx.recv().await else { return; };
                let version = match outcome {
                    crate::rip::makemkv::ProbeOutcome::Ok(v)
                    | crate::rip::makemkv::ProbeOutcome::Outdated(v) => v,
                    crate::rip::makemkv::ProbeOutcome::Missing => return,
                };
                if version.supports_mvc() {
                    return;
                }
                if let Some(window) = page.parent_window() {
                    window.add_toast(
                        adw::Toast::builder()
                            .title(&format!(
                                "MakeMKV v{}.{}.{} drops the MVC track on 3D Blu-rays. \
                                 Upgrade to v1.18+ for 3D rips to include the dependent view.",
                                version.major, version.minor, version.patch
                            ))
                            .timeout(12)
                            .build(),
                    );
                }
            }
        ));
    }

    /// Set the persistent identification banner from the lookup outcome.
    /// Replaces the old transient toast as the durable on-page status:
    /// identified / catalogue-unreachable / not-catalogued. For the
    /// not-catalogued case it also opens the submission form, since that's
    /// the next thing the user would do.
    fn update_status_banner(&self, result: &IdentificationResult) {
        use crate::identify::pipeline::LookupStatus;
        let banner = self.imp().status_banner.get();
        banner.set_button_label(None);
        let kind = crate::window::describe_disc_type(result.disc_type);

        if let Some(identity) = result.identities.first() {
            // Identified: show the rich poster card, hide the banner.
            let year = identity.year.map(|y| format!(" ({y})")).unwrap_or_default();
            let title = if identity.item_title.trim().is_empty() {
                identity.release_slug.clone()
            } else {
                identity.item_title.clone()
            };
            self.imp().identity_title.set_label(&format!("{title}{year}"));
            let mut sub = vec![kind.to_string()];
            if !identity.release_slug.trim().is_empty() {
                sub.push(identity.release_slug.clone());
            }
            self.imp().identity_subtitle.set_label(&sub.join("  •  "));
            self.imp().identity_group.set_visible(true);
            banner.set_revealed(false);
            match identity.cover_art_url() {
                Some(url) => self.load_poster(&url),
                None => self.imp().poster_image.set_visible(false),
            }
            return;
        }

        // Not identified: no poster card, show the status banner.
        self.imp().identity_group.set_visible(false);
        match &result.lookup_status {
            LookupStatus::Failed(_) => {
                // Could not reach the catalogue — the disc may well be in
                // it. Don't claim "not catalogued"; offer to add details.
                banner.set_title(&format!(
                    "{kind} • TheDiscDB unreachable — titles below are from the disc scan. \
                     You can still rip; identification is just missing."
                ));
                banner.set_button_label(Some("Add details"));
                banner.set_revealed(true);
            }
            LookupStatus::Ok => {
                // Looked up successfully and genuinely not catalogued.
                banner.set_title(&format!(
                    "{kind} • Not in TheDiscDB. Ripping works; add the details to submit this disc."
                ));
                banner.set_button_label(Some("Add details"));
                banner.set_revealed(true);
                self.imp().submission_expander.set_expanded(true);
            }
            LookupStatus::NotAttempted => {
                // No content hash (standalone MKV, or disc not mounted).
                banner.set_revealed(false);
            }
        }

        // The banner's action button reveals the submission form.
        banner.connect_button_clicked(clone!(
            #[weak(rename_to = page)]
            self,
            move |_| page.imp().submission_expander.set_expanded(true)
        ));
    }

    /// Download the identified disc's cover art and paint it into the
    /// poster Picture. Best-effort: on any failure the poster is hidden so
    /// the card collapses to just the title text. Mirrors the async
    /// download → gdk::Texture pattern used by the cover-art picker.
    fn load_poster(&self, url: &str) {
        let picture = self.imp().poster_image.get();
        picture.set_visible(true);
        picture.set_paintable(gtk::gdk::Paintable::NONE);
        let url = url.to_string();
        let (tx, rx) = async_channel::bounded::<Option<Vec<u8>>>(1);
        crate::runtime::tokio_runtime().spawn(async move {
            let bytes = async {
                let resp = reqwest::get(&url).await.ok()?.error_for_status().ok()?;
                resp.bytes().await.ok().map(|b| b.to_vec())
            }
            .await;
            let _ = tx.send(bytes).await;
        });
        glib::MainContext::default().spawn_local(clone!(
            #[weak]
            picture,
            async move {
                let Ok(Some(bytes)) = rx.recv().await else {
                    picture.set_visible(false);
                    return;
                };
                match gtk::gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) {
                    Ok(tex) => picture.set_paintable(Some(&tex)),
                    Err(e) => {
                        tracing::warn!("decode cover art failed: {e}");
                        picture.set_visible(false);
                    }
                }
            }
        ));
    }

    fn populate_with_identity(&self, result: &IdentificationResult) {
        self.update_status_banner(result);
        let group = self.imp().title_group.get();
        group.set_title(&format_group_title(result));
        group.set_description(Some(&format_group_description(result)));
        self.imp().identities.replace(result.identities.clone());
        self.imp()
            .content_hash
            .replace(result.content_hash.clone().unwrap_or_default());
        // Stash bdmt for later use (pre-fill of per-title display
        // titles + the title-detail page).
        self.imp().bdmt.replace(result.bdmt.clone());
        // Pre-fill submission metadata fields from the matched
        // TheDiscDB identity (when there is one) so corrections
        // start from the existing record rather than blank.
        if let Some(identity) = result.identities.first() {
            if let Some(y) = identity.year {
                self.imp().year_row.set_value(y as f64);
            }
            if let Some(t) = identity.tmdb_id {
                self.imp().tmdb_id_row.set_text(&t.to_string());
            }
            if let Some(i) = &identity.imdb_id {
                self.imp().imdb_id_row.set_text(i);
            }
            self.imp()
                .release_slug_row
                .set_text(&identity.release_slug);
        }
        // DVD region pre-fill. Pulled from VIDEO_TS.IFO byte 0x23 via
        // identify::dvd. Only fires when the editor row is still empty
        // so a user edit isn't clobbered on re-population.
        if let Some(region) = &result.dvd_region_code {
            if self.imp().region_code_row.text().trim().is_empty() {
                self.imp().region_code_row.set_text(region);
            }
        }
        if let Some(name) = &result.scan.disc.name {
            self.set_title(name);
        }
        let detected = auto_detect_content_kind(&result.scan.titles);
        self.imp().series_toggle.set_active(detected == DiscContentKind::Series);

        // Pre-fill the editable title and season fields. Precedence:
        // bdmt_eng.xml disc title beats the parsed UDF disc label,
        // because the publisher authored bdmt. The label is a
        // backup parsing path.
        let bdmt_title = result.bdmt.as_ref().and_then(|b| b.disc_title.clone());
        if let Some(t) = &bdmt_title {
            self.imp().title_override.set_text(t);
        } else if let Some(disc_name) = &result.scan.disc.name {
            let (guess_title, guess_season) = parse_series_label(disc_name);
            self.imp().title_override.set_text(&guess_title);
            if let Some(s) = guess_season {
                self.imp().season_override.set_value(s as f64);
            }
        }

        self.populate_rows(&result.scan);
        self.imp().series_toggle.connect_active_notify(clone!(
            #[weak(rename_to = page)]
            self,
            move |toggle| page.set_series_widgets_visible(toggle.is_active())
        ));
        self.set_series_widgets_visible(detected == DiscContentKind::Series);
    }

    fn set_series_widgets_visible(&self, visible: bool) {
        self.imp().season_override.set_visible(visible);
        for entry in self.imp().episode_entries.borrow().iter() {
            entry.set_visible(visible);
        }
    }

    fn populate_rows(&self, scan: &MakemkvScan) {
        let group = self.imp().title_group.get();

        // First identity (highest-confidence TheDiscDB match) provides
        // per-title roles + display titles. Match by sourceFile, not
        // index -- TheDiscDB's index is its own ordering and does not
        // line up with MakeMKV's title index.
        let identities = self.imp().identities.borrow();
        let identity_titles: Vec<TitleIdentity> = identities
            .first()
            .map(|i| i.titles.clone())
            .unwrap_or_default();
        drop(identities);
        let role_of = |t: &TitleAttributes| -> Option<TitleRole> {
            crate::rip::plan::match_identity_for(&identity_titles, t).map(|i| i.role)
        };

        // Display order. For a movie, float the main feature to the top so
        // it's the obvious thing to tick; extras follow. A *stable* sort
        // keeps everything else in scan order, so series episodes (kept in
        // scan order) and within-group ordering are preserved. The "main"
        // is the title TheDiscDB tags Main, or — when nothing is
        // identified — the longest title.
        let mut ordered: Vec<TitleAttributes> = scan.titles.clone();
        if auto_detect_content_kind(&scan.titles) == DiscContentKind::Movie {
            let any_main = scan.titles.iter().any(|t| role_of(t) == Some(TitleRole::Main));
            let longest = scan
                .titles
                .iter()
                .max_by_key(|t| t.duration_seconds.unwrap_or(0))
                .map(|t| t.index);
            ordered.sort_by_key(|t| {
                let is_main = role_of(t) == Some(TitleRole::Main)
                    || (!any_main && Some(t.index) == longest);
                u8::from(!is_main)
            });
        }
        // Keep `titles` parallel to the rows/checkboxes we build below.
        self.imp().titles.replace(ordered.clone());

        let pairs: Vec<(u32, &str)> = ordered
            .iter()
            .map(|t| (t.index, t.segment_map.as_deref().unwrap_or("")))
            .collect();
        let relations = analyze_relations(&pairs);

        // Per-title display titles from bdmt_eng.xml, keyed by the
        // BDA `titleNumber` attribute (1-based -> MakeMKV index +1).
        let bdmt = self.imp().bdmt.borrow();
        let bdmt_names = bdmt.as_ref().map(|b| &b.title_names);

        let mut checkboxes = Vec::with_capacity(ordered.len());
        let mut episode_entries = Vec::with_capacity(ordered.len());

        for (t, relation) in ordered.iter().zip(relations.iter()) {
            let identity = crate::rip::plan::match_identity_for(&identity_titles, t);
            let role = identity.map(|i| i.role);
            let identity_display = identity
                .map(|i| i.display_title.as_str())
                .filter(|s| !s.is_empty());
            // bdmt titles are 1-based; MakeMKV titles are 0-based. Try
            // both mappings since some authoring tools differ.
            let bdmt_display = bdmt_names.and_then(|m| {
                m.get(&(t.index + 1))
                    .or_else(|| m.get(&t.index))
                    .map(|s| s.as_str())
            });

            // Row title: precedence is TheDiscDB > bdmt > MakeMKV name.
            let title_label = match (identity_display, bdmt_display, t.name.as_deref()) {
                (Some(dt), _, _) => format!("Title {} — {}", t.index, dt),
                (None, Some(bd), _) => format!("Title {} — {}", t.index, bd),
                (None, None, Some(n)) if !n.is_empty() => {
                    format!("Title {} — {}", t.index, n)
                }
                _ => format!("Title {}", t.index),
            };

            // Subtitle: role • duration • size (+ relation when notable).
            // The raw source-file name lives on the detail page now — it's
            // developer-facing and just lengthened every row.
            let mut subtitle_parts: Vec<String> = Vec::with_capacity(4);
            if let Some(r) = role {
                subtitle_parts.push(role_badge(r).to_string());
            }
            subtitle_parts.push(format_duration(t.duration_seconds.unwrap_or(0)));
            subtitle_parts.push(format_bytes(t.size_bytes.unwrap_or(0)));
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

            // 3D marker only. The per-title 3D output format moved to the
            // detail page (the edit button) to keep rows uncluttered.
            if t.has_mvc_stream() {
                let badge = gtk::Label::builder()
                    .label("3D")
                    .valign(gtk::Align::Center)
                    .css_classes(["pill", "accent"])
                    .tooltip_text("Stereoscopic-3D (MVC). Set the output format via the edit button.")
                    .build();
                row.add_suffix(&badge);
            }

            let episode_entry = gtk::Entry::builder()
                .placeholder_text("Episode title (optional)")
                .valign(gtk::Align::Center)
                .width_chars(28)
                .visible(false)
                .build();
            row.add_suffix(&episode_entry);

            // Edit affordance: small icon button on the right that
            // opens the per-title detail page. Doesn't interfere with
            // the row's primary activation (which toggles the rip
            // checkbox).
            let edit_button = gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("Edit title details (display name, role, 3D format, chapters)")
                .css_classes(["flat"])
                .build();
            let title_index = t.index;
            edit_button.connect_clicked(clone!(
                #[weak(rename_to = page)]
                self,
                move |_| page.open_title_detail(title_index)
            ));
            row.add_suffix(&edit_button);

            group.add(&row);
            checkboxes.push(check);
            episode_entries.push(episode_entry);
        }

        self.imp().checkboxes.replace(checkboxes);
        self.imp().episode_entries.replace(episode_entries);
        self.install_select_buttons();
        self.refresh_rip_sensitivity();
    }

    /// Quick-select buttons in the title-group header: tick the main
    /// feature, everything, or nothing. Installed once per population.
    fn install_select_buttons(&self) {
        let bx = gtk::Box::builder().spacing(6).build();
        let main_btn = gtk::Button::builder()
            .label("Main")
            .tooltip_text("Select only the main feature (the first / longest title)")
            .css_classes(["flat"])
            .build();
        let all_btn = gtk::Button::builder().label("All").css_classes(["flat"]).build();
        let none_btn = gtk::Button::builder().label("None").css_classes(["flat"]).build();
        main_btn.connect_clicked(clone!(
            #[weak(rename_to = page)]
            self,
            move |_| page.select_titles(SelectMode::Main)
        ));
        all_btn.connect_clicked(clone!(
            #[weak(rename_to = page)]
            self,
            move |_| page.select_titles(SelectMode::All)
        ));
        none_btn.connect_clicked(clone!(
            #[weak(rename_to = page)]
            self,
            move |_| page.select_titles(SelectMode::None)
        ));
        bx.append(&main_btn);
        bx.append(&all_btn);
        bx.append(&none_btn);
        self.imp().title_group.set_header_suffix(Some(&bx));
    }

    /// Apply a quick-select mode to the checkboxes. `Main` ticks just the
    /// first row (the planner / sort put the main feature there).
    fn select_titles(&self, mode: SelectMode) {
        let checks = self.imp().checkboxes.borrow();
        for (i, c) in checks.iter().enumerate() {
            let on = match mode {
                SelectMode::All => true,
                SelectMode::None => false,
                SelectMode::Main => i == 0,
            };
            c.set_active(on);
        }
    }

    fn refresh_rip_sensitivity(&self) {
        let any_checked = self
            .imp()
            .checkboxes
            .borrow()
            .iter()
            .any(|c| c.is_active());
        self.imp().process_button.set_sensitive(any_checked);
    }

    /// Open the per-title detail page (display title / role / chapter
    /// edits) for the title with the given MakeMKV index. Pulls
    /// existing edits + TheDiscDB-provided defaults to pre-populate.
    fn open_title_detail(&self, title_index: u32) {
        use crate::rip::plan::match_identity_for;
        use crate::ui::title_detail_page::{TitleDetailPage, TitleEdit};

        let titles = self.imp().titles.borrow();
        let Some(title_attr) = titles.iter().find(|t| t.index == title_index) else {
            return;
        };
        let identities = self.imp().identities.borrow();
        let identity_titles = identities
            .first()
            .map(|i| i.titles.as_slice())
            .unwrap_or(&[]);
        let identity = match_identity_for(identity_titles, title_attr);

        let bdmt = self.imp().bdmt.borrow();
        let bdmt_default = bdmt.as_ref().and_then(|b| {
            b.title_names
                .get(&(title_index + 1))
                .or_else(|| b.title_names.get(&title_index))
                .cloned()
        });
        let display_default = identity
            .map(|i| i.display_title.clone())
            .filter(|s| !s.is_empty())
            .or(bdmt_default)
            .or_else(|| title_attr.name.clone())
            .unwrap_or_else(|| format!("Title {}", title_index));

        let chapter_defaults: Vec<String> = identity
            .map(|i| {
                let mut chs = i.chapters.clone();
                chs.sort_by_key(|c| c.index);
                chs.into_iter().map(|c| c.title).collect()
            })
            .unwrap_or_default();

        let existing_edit = self
            .imp()
            .title_edits
            .borrow()
            .get(&title_index)
            .cloned()
            .unwrap_or_else(|| TitleEdit {
                title_index,
                ..Default::default()
            });

        let is_3d = title_attr.has_mvc_stream();
        let detail = TitleDetailPage::default();
        detail.populate(&existing_edit, &display_default, &chapter_defaults, is_3d);
        detail.connect_saved(clone!(
            #[weak(rename_to = page)]
            self,
            move |edit| {
                page.imp()
                    .title_edits
                    .borrow_mut()
                    .insert(edit.title_index, edit);
            }
        ));

        if let Some(nav) = navigation_view(self) {
            nav.push(&detail);
        }
    }

    fn setup_actions(&self) {
        let process_action = gio::SimpleAction::new("process-selected", None);
        process_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.start_processing()
        ));
        let sonarr_action = gio::SimpleAction::new("sonarr-lookup", None);
        sonarr_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.start_sonarr_lookup()
        ));
        let submit_action = gio::SimpleAction::new("submit-corrections", None);
        submit_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.submit_corrections()
        ));
        let tmdb_action = gio::SimpleAction::new("lookup-tmdb", None);
        tmdb_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.start_tmdb_lookup()
        ));
        let imdb_action = gio::SimpleAction::new("lookup-imdb", None);
        imdb_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.start_imdb_lookup()
        ));
        let upc_action = gio::SimpleAction::new("lookup-upc", None);
        upc_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.start_upc_lookup()
        ));
        let cover_action = gio::SimpleAction::new("pick-cover-art", None);
        cover_action.connect_activate(clone!(
            #[weak(rename_to = page)]
            self,
            move |_, _| page.start_cover_art_pick()
        ));

        let group = gio::SimpleActionGroup::new();
        group.add_action(&process_action);
        group.add_action(&sonarr_action);
        group.add_action(&submit_action);
        group.add_action(&tmdb_action);
        group.add_action(&imdb_action);
        group.add_action(&upc_action);
        group.add_action(&cover_action);
        self.insert_action_group("page", Some(&group));
    }

    /// Read the disc-metadata UI rows into a (MovieMetadata,
    /// ReleaseMetadata) pair. Empty optional fields → `None`.
    fn read_submission_metadata(
        &self,
    ) -> (
        crate::identify::submission::MovieMetadata,
        crate::identify::submission::ReleaseMetadata,
    ) {
        use crate::identify::submission::{
            ContentType, MovieMetadata, ReleaseMetadata,
        };
        let title = self.imp().title_override.text().trim().to_string();
        let year_raw = self.imp().year_row.value() as u32;
        let year = if (1900..=2100).contains(&year_raw) {
            Some(year_raw)
        } else {
            None
        };
        let opt = |row: &adw::EntryRow| {
            let s = row.text().trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let movie = MovieMetadata {
            title: title.clone(),
            year,
            content_type: if self.imp().series_toggle.is_active() {
                ContentType::Series
            } else {
                ContentType::Movie
            },
            plot: opt(&self.imp().plot_row),
            tagline: opt(&self.imp().tagline_row),
            tmdb_id: opt(&self.imp().tmdb_id_row).and_then(|s| s.parse().ok()),
            imdb_id: opt(&self.imp().imdb_id_row),
            tvdb_id: None,
        };
        let release = ReleaseMetadata {
            slug: opt(&self.imp().release_slug_row).unwrap_or_else(|| "blu-ray".into()),
            title: opt(&self.imp().release_title_row).unwrap_or_else(|| {
                match year {
                    Some(y) => format!("{y} Blu-ray"),
                    None => "Blu-ray".into(),
                }
            }),
            year,
            locale: opt(&self.imp().locale_row),
            region_code: opt(&self.imp().region_code_row),
            upc: opt(&self.imp().upc_row),
            asin: opt(&self.imp().asin_row),
            image_url: None,
            release_date: None,
            contributors: Vec::new(),
            groups: Vec::new(),
        };
        (movie, release)
    }

    /// Stage a `disc0N.json` for TheDiscDB, surface the path in a
    /// toast, and open the data-repo GitHub page so the user can
    /// open a PR. Hash + identity come from the original scan;
    /// per-title edits from the title-detail page state.
    fn submit_corrections(&self) {
        use crate::identify::submission::{
            github_repo_url, open_in_browser, stage_full_submission_with, stage_submission,
            DiscSubmission, SubmissionArtifacts,
        };
        let disc_name = self
            .imp()
            .disc_name
            .borrow()
            .clone()
            .unwrap_or_else(|| "Unknown disc".to_string());
        let identities = self.imp().identities.borrow();
        let identity = identities.first();
        let content_hash = self.imp().content_hash.borrow().clone();
        let scan_snapshot = {
            let titles = self.imp().titles.borrow().clone();
            crate::rip::makemkv_parse::MakemkvScan {
                disc: crate::rip::makemkv_parse::DiscAttributes {
                    name: Some(disc_name.clone()),
                    ..Default::default()
                },
                titles,
                ..Default::default()
            }
        };
        let disc = DiscSubmission {
            disc_index: identity.map(|i| i.disc_index).unwrap_or(1),
            disc_slug: identity
                .map(|i| i.release_slug.clone())
                .unwrap_or_else(|| "blu-ray".into()),
            disc_name: disc_name.clone(),
            format: "Blu-Ray".to_string(),
            content_hash: if content_hash.is_empty() {
                // Stand-alone MKV or otherwise-unidentified input
                // doesn't have a TheDiscDB content hash. Stage under
                // "unhashed/<disc_name>" so the user can still find
                // it and figure out where to put it manually.
                format!("unhashed-{}", disc_name.replace('/', "_"))
            } else {
                content_hash
            },
            comment: None,
        };
        let edits = self.imp().title_edits.borrow().clone();
        // If the user has filled in disc-level metadata (year, IDs,
        // plot, etc), we stage the full new-disc submission tree
        // (metadata.json + release.json + disc0N.json) rather than
        // just the per-disc corrections file.
        let (movie, release) = self.read_submission_metadata();
        let artifacts = SubmissionArtifacts {
            cover_jpg: self.imp().picked_cover_jpg.borrow().clone(),
            ..SubmissionArtifacts::default()
        };
        let staged = if !movie.title.is_empty()
            && (movie.year.is_some()
                || movie.tmdb_id.is_some()
                || movie.imdb_id.is_some())
        {
            stage_full_submission_with(
                &movie, &release, &disc, &scan_snapshot, identity, &edits, &artifacts,
            )
        } else {
            stage_submission(&disc, &scan_snapshot, identity, &edits)
        };
        match staged {
            Ok(path) => {
                let msg = format!(
                    "Staged TheDiscDB submission at {} — opening data repo…",
                    path.display()
                );
                if let Some(window) = self.parent_window() {
                    window.add_toast(
                        adw::Toast::builder().title(&msg).timeout(8).build(),
                    );
                }
                if let Err(e) = open_in_browser(github_repo_url()) {
                    tracing::warn!("xdg-open failed: {e:#}");
                    if let Some(window) = self.parent_window() {
                        window.add_toast(
                            adw::Toast::builder()
                                .title(&format!(
                                    "Couldn't open browser; visit {}",
                                    github_repo_url()
                                ))
                                .timeout(8)
                                .build(),
                        );
                    }
                }
            }
            Err(e) => {
                tracing::error!("submission staging failed: {e:#}");
                if let Some(window) = self.parent_window() {
                    window.add_toast(
                        adw::Toast::builder()
                            .title(&format!("Submission staging failed: {e}"))
                            .timeout(8)
                            .build(),
                    );
                }
            }
        }
    }

    /// Per-title 3D output format for the *selected* titles, read from the
    /// per-title edits set on the detail page. Title index → chosen layout;
    /// titles left on "None" (or 2D titles) are absent. Fed to `plan_rip`
    /// so each title converts independently.
    fn format_by_index(&self) -> HashMap<u32, OutputFormat> {
        let titles = self.imp().titles.borrow();
        let checks = self.imp().checkboxes.borrow();
        let edits = self.imp().title_edits.borrow();
        let mut map = HashMap::new();
        for (i, t) in titles.iter().enumerate() {
            if !checks.get(i).map_or(false, |c| c.is_active()) {
                continue;
            }
            if let Some(fmt) = edits.get(&t.index).and_then(|e| e.format) {
                map.insert(t.index, fmt);
            }
        }
        map
    }

    /// The standalone-MKV convert path has a single title and no rip step;
    /// use the format chosen for it on the detail page.
    fn mkv_convert_format(&self) -> Option<OutputFormat> {
        let titles = self.imp().titles.borrow();
        let edits = self.imp().title_edits.borrow();
        titles
            .first()
            .and_then(|t| edits.get(&t.index))
            .and_then(|e| e.format)
    }

    /// Encoder backend — now a global preference (Settings), not per disc.
    fn selected_hw_backend(&self) -> crate::convert::hw::HwBackend {
        crate::settings::settings()
            .lock()
            .expect("settings mutex")
            .conversion_hw_backend()
    }

    fn selected_codec(&self) -> crate::convert::hw::EncodeCodec {
        crate::settings::settings()
            .lock()
            .expect("settings mutex")
            .conversion_codec()
    }

    /// Single entry point for the header's `Process Selected` button.
    /// Dispatches based on input type: physical disc / ISO go through
    /// the rip pipeline (the orchestrator); a standalone MKV with a
    /// selected 3D output format goes through the convert pipeline.
    fn start_processing(&self) {
        let is_already_extracted = self.imp().iso_path.borrow().is_some()
            && self.imp().source.borrow().as_ref().map_or(false, |s| {
                matches!(s, crate::rip::makemkv::ScanSource::Iso(p) if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mkv")))
            });
        let format = self.mkv_convert_format();

        if is_already_extracted {
            // MKV input: there's nothing to "rip"; processing means
            // running the 3D conversion. If the user left the format
            // on "Off" there's no work to do.
            match format {
                Some(fmt) => self.start_conversion(fmt),
                None => {
                    if let Some(window) = self.parent_window() {
                        window.add_toast(
                            adw::Toast::builder()
                                .title("Pick a 3D output format to convert this MKV.")
                                .timeout(4)
                                .build(),
                        );
                    }
                }
            }
            return;
        }

        // Physical disc / ISO: run the rip pipeline. The orchestrator
        // chains a post-rip 3D conversion when conversion_format is
        // set on the queue item (we plumb it via NamingOpts ->
        // PlannedTitle -> RipQueueItem), so a single Process click
        // produces both the raw MVC MKV and the FSBS/HSBS/etc. output.
        self.start_rip();
    }

    /// Look up by TMDb ID using the value currently in tmdb_id_row.
    /// Applies title / year / plot / tagline / imdb_id back to the
    /// submission fields when the call succeeds.
    fn start_tmdb_lookup(&self) {
        let id_text = self.imp().tmdb_id_row.text().trim().to_string();
        let Ok(id) = id_text.parse::<u64>() else {
            self.toast_in_window("Enter a numeric TMDb ID first.");
            return;
        };
        let series = self.imp().series_toggle.is_active();
        self.do_tmdb_lookup(Box::new(move |c| Box::pin(async move {
            if series { c.fetch_series(id).await } else { c.fetch_movie(id).await }
        })));
    }

    /// Look up by IMDb ID using the value currently in imdb_id_row.
    /// Routes through TMDB's /find endpoint (no separate IMDb API).
    fn start_imdb_lookup(&self) {
        let id_text = self.imp().imdb_id_row.text().trim().to_string();
        if id_text.is_empty() {
            self.toast_in_window("Enter an IMDb ID (tt…) first.");
            return;
        }
        self.do_tmdb_lookup(Box::new(move |c| Box::pin(async move {
            c.fetch_by_imdb_id(&id_text).await
        })));
    }

    /// Shared dispatch: pull the API key from settings, run the
    /// requester on the tokio runtime, apply the result to the
    /// submission fields on the GTK main thread.
    fn do_tmdb_lookup(
        &self,
        request: Box<
            dyn FnOnce(
                    crate::identify::tmdb::TmdbClient,
                )
                    -> std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = anyhow::Result<crate::identify::tmdb::TmdbDetails>,
                                > + Send,
                        >,
                    >
                + Send,
        >,
    ) {
        let key = crate::settings::settings()
            .lock()
            .expect("settings mutex")
            .tmdb_api_key
            .clone()
            .unwrap_or_default();
        let key = key.trim().to_string();
        if key.is_empty() {
            self.toast_in_window(
                "Set the TMDB API key in Preferences first (free, from themoviedb.org).",
            );
            return;
        }
        let (tx, rx) = async_channel::bounded::<anyhow::Result<crate::identify::tmdb::TmdbDetails>>(1);
        crate::runtime::tokio_runtime().spawn(async move {
            let client = crate::identify::tmdb::TmdbClient::new(key);
            let _ = tx.send(request(client).await).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = page)]
            self,
            async move {
                match rx.recv().await {
                    Ok(Ok(d)) => page.apply_tmdb_details(&d),
                    Ok(Err(e)) => {
                        tracing::error!("TMDB lookup failed: {e:#}");
                        page.toast_in_window(&format!("TMDB lookup failed: {e}"));
                    }
                    Err(e) => {
                        tracing::error!("TMDB channel closed: {e}");
                    }
                }
            }
        ));
    }

    /// Fill the submission fields from a TMDB lookup result.
    fn apply_tmdb_details(&self, d: &crate::identify::tmdb::TmdbDetails) {
        if let Some(t) = &d.title {
            self.imp().title_override.set_text(t);
        }
        if let Some(y) = d.year {
            self.imp().year_row.set_value(y as f64);
        }
        if let Some(p) = &d.plot {
            self.imp().plot_row.set_text(p);
        }
        if let Some(t) = &d.tagline {
            self.imp().tagline_row.set_text(t);
        }
        if let Some(id) = d.tmdb_id {
            self.imp().tmdb_id_row.set_text(&id.to_string());
        }
        if let Some(i) = &d.imdb_id {
            self.imp().imdb_id_row.set_text(i);
        }
        if let Some(ct) = d.content_type {
            self.imp().series_toggle.set_active(ct == "Series");
        }
        if let Some(window) = self.parent_window() {
            window.add_toast(
                adw::Toast::builder()
                    .title("TMDB details applied to submission fields.")
                    .timeout(3)
                    .build(),
            );
        }
    }

    /// Look up the value currently in `upc_row` via UPCitemDB. Pre-
    /// fills the ASIN and `release_title` rows when both are empty so
    /// a user edit isn't clobbered. Free trial tier -- no API key.
    fn start_upc_lookup(&self) {
        let upc = self.imp().upc_row.text().trim().to_string();
        if upc.is_empty() {
            self.toast_in_window("Enter the UPC printed on the disc packaging first.");
            return;
        }
        let (tx, rx) = async_channel::bounded::<anyhow::Result<crate::identify::upc::UpcDetails>>(1);
        crate::runtime::tokio_runtime().spawn(async move {
            let client = crate::identify::upc::UpcClient::new();
            let _ = tx.send(client.lookup(&upc).await).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = page)]
            self,
            async move {
                match rx.recv().await {
                    Ok(Ok(d)) => page.apply_upc_details(&d),
                    Ok(Err(e)) => {
                        tracing::error!("UPC lookup failed: {e:#}");
                        page.toast_in_window(&format!("UPC lookup failed: {e}"));
                    }
                    Err(e) => tracing::error!("UPC channel closed: {e}"),
                }
            }
        ));
    }

    fn apply_upc_details(&self, d: &crate::identify::upc::UpcDetails) {
        if let Some(asin) = &d.asin {
            if self.imp().asin_row.text().trim().is_empty() {
                self.imp().asin_row.set_text(asin);
            }
        }
        if let Some(title) = &d.title {
            if self.imp().release_title_row.text().trim().is_empty() {
                self.imp().release_title_row.set_text(title);
            }
        }
        let summary = match (&d.asin, &d.brand) {
            (Some(a), Some(b)) => format!("UPC matched: {b} (ASIN {a})"),
            (Some(a), None) => format!("UPC matched (ASIN {a})"),
            (None, Some(b)) => format!("UPC matched: {b}"),
            (None, None) => "UPC matched (no ASIN or brand on file)".to_string(),
        };
        if let Some(window) = self.parent_window() {
            window.add_toast(adw::Toast::builder().title(&summary).timeout(4).build());
        }
    }

    /// Open the cover-art picker. Requires a TMDB ID + a TMDB API
    /// key; toasts when either is missing. The picker presents the
    /// highest-voted posters; on confirm we download the full-res
    /// JPEG and stash its bytes for the submission stager to attach
    /// as cover.jpg.
    fn start_cover_art_pick(&self) {
        let id_text = self.imp().tmdb_id_row.text().trim().to_string();
        let Ok(tmdb_id) = id_text.parse::<u64>() else {
            self.toast_in_window("Enter a numeric TMDb ID first.");
            return;
        };
        let key = crate::settings::settings()
            .lock()
            .expect("settings mutex")
            .tmdb_api_key
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string();
        if key.is_empty() {
            self.toast_in_window(
                "Set the TMDB API key in Preferences first (free, from themoviedb.org).",
            );
            return;
        }
        let kind = if self.imp().series_toggle.is_active() { "tv" } else { "movie" };
        let dialog = crate::ui::cover_art_picker_dialog::CoverArtPickerDialog::default();

        // Wire the confirm callback before the network request so a
        // very fast response doesn't fire before we're listening.
        dialog.on_confirm(clone!(
            #[weak(rename_to = page)] self,
            move |picked| page.download_picked_cover(picked, &key)
        ));

        if let Some(root) = self.root() {
            if let Some(window) = root.downcast::<gtk::Window>().ok() {
                dialog.present(Some(&window));
            } else {
                dialog.present(None::<&gtk::Widget>);
            }
        } else {
            dialog.present(None::<&gtk::Widget>);
        }

        // Fetch the image list and hand it to the dialog. We don't
        // chain `load_posters` into the dialog's own ctor because
        // tmdb fetch is async and the dialog should already be open
        // showing its spinner state.
        let key2 = crate::settings::settings()
            .lock()
            .expect("settings mutex")
            .tmdb_api_key
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string();
        let (tx, rx) =
            async_channel::bounded::<anyhow::Result<crate::identify::tmdb::TmdbImageSet>>(1);
        let kind = kind.to_string();
        crate::runtime::tokio_runtime().spawn(async move {
            let client = crate::identify::tmdb::TmdbClient::new(key2);
            let _ = tx.send(client.fetch_images(&kind, tmdb_id).await).await;
        });
        glib::MainContext::default().spawn_local(clone!(
            #[weak] dialog,
            #[weak(rename_to = page)] self,
            async move {
                match rx.recv().await {
                    Ok(Ok(set)) => dialog.load_posters(set.posters),
                    Ok(Err(e)) => {
                        tracing::warn!("TMDB images fetch failed: {e:#}");
                        page.toast_in_window(&format!("Could not fetch posters: {e}"));
                        let _ = dialog.close();
                    }
                    Err(_) => {
                        let _ = dialog.close();
                    }
                }
            }
        ));
    }

    fn download_picked_cover(
        &self,
        picked: crate::identify::tmdb::TmdbImage,
        api_key: &str,
    ) {
        let key = api_key.to_string();
        let (tx, rx) = async_channel::bounded::<anyhow::Result<Vec<u8>>>(1);
        crate::runtime::tokio_runtime().spawn(async move {
            let client = crate::identify::tmdb::TmdbClient::new(key);
            let _ = tx
                .send(client.download_image(&picked.file_path, "original").await)
                .await;
        });
        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = page)] self,
            async move {
                match rx.recv().await {
                    Ok(Ok(bytes)) => {
                        let summary = format!(
                            "Cover art picked ({} KB) — will ship as cover.jpg",
                            bytes.len() / 1024
                        );
                        page.imp().picked_cover_jpg.replace(Some(bytes));
                        page.toast_in_window(&summary);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("download cover failed: {e:#}");
                        page.toast_in_window(&format!("Cover download failed: {e}"));
                    }
                    Err(_) => {}
                }
            }
        ));
    }

    fn start_sonarr_lookup(&self) {
        // Dispatch by series-toggle state: Sonarr for series, Radarr for
        // movies. Both look up using the current title_override text.
        if self.imp().series_toggle.is_active() {
            self.start_sonarr_lookup_inner();
        } else {
            self.start_radarr_lookup_inner();
        }
    }

    fn start_sonarr_lookup_inner(&self) {
        let cfg = crate::settings::settings()
            .lock()
            .expect("settings mutex")
            .sonarr
            .clone();
        if !cfg.is_configured() {
            self.toast_in_window("Sonarr URL and API key not set — see Preferences.");
            return;
        }
        let url = cfg.url.expect("checked is_configured");
        let api_key = cfg.api_key.expect("checked is_configured");

        let term = self.imp().title_override.text().to_string();
        if term.trim().is_empty() {
            self.toast_in_window("Enter a series title first.");
            return;
        }
        let season = self.imp().season_override.value().max(0.0) as u32;

        let (tx, rx) =
            async_channel::bounded::<anyhow::Result<(String, Vec<(u32, String)>)>>(1);
        let term_for_task = term.clone();
        crate::runtime::tokio_runtime().spawn(async move {
            let result = async {
                let client = SonarrClient::from_config(&url, &api_key)?;
                let candidates = client.lookup(&term_for_task).await?;
                let chosen = candidates
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no Sonarr match for \"{term_for_task}\""))?;
                let canonical_title = chosen.title.clone();
                let mut episodes = client.episodes(chosen.id, Some(season)).await?;
                episodes.sort_by_key(|e| e.episode_number);
                let pairs: Vec<(u32, String)> = episodes
                    .into_iter()
                    .filter_map(|e| e.title.map(|t| (e.episode_number, t)))
                    .collect();
                anyhow::Ok((canonical_title, pairs))
            }
            .await;
            let _ = tx.send(result).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = page)]
            self,
            async move {
                let result = rx.recv().await;
                match result {
                    Ok(Ok((canonical, pairs))) => {
                        page.imp().title_override.set_text(&canonical);
                        let count = pairs.len();
                        page.apply_episode_titles(pairs);
                        page.toast_in_window(&format!(
                            "Sonarr: matched \"{canonical}\", filled {count} episode title(s)"
                        ));
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Sonarr lookup failed: {e:#}");
                        page.toast_in_window(&format!("Sonarr lookup failed: {e}"));
                    }
                    Err(e) => {
                        tracing::error!("Sonarr channel closed: {e}");
                    }
                }
            }
        ));
    }

    fn start_radarr_lookup_inner(&self) {
        let cfg = crate::settings::settings()
            .lock()
            .expect("settings mutex")
            .radarr
            .clone();
        if !cfg.is_configured() {
            self.toast_in_window("Radarr URL and API key not set — see Preferences.");
            return;
        }
        let url = cfg.url.expect("checked is_configured");
        let api_key = cfg.api_key.expect("checked is_configured");

        let term = self.imp().title_override.text().to_string();
        if term.trim().is_empty() {
            self.toast_in_window("Enter a movie title first.");
            return;
        }

        let (tx, rx) = async_channel::bounded::<anyhow::Result<String>>(1);
        let term_for_task = term.clone();
        crate::runtime::tokio_runtime().spawn(async move {
            let result = async {
                let client = RadarrClient::from_config(&url, &api_key)?;
                let candidates = client.lookup(&term_for_task).await?;
                let chosen = candidates
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no Radarr match for \"{term_for_task}\""))?;
                let with_year = match chosen.year {
                    Some(y) => format!("{} ({y})", chosen.title),
                    None => chosen.title,
                };
                anyhow::Ok(with_year)
            }
            .await;
            let _ = tx.send(result).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = page)]
            self,
            async move {
                let result = rx.recv().await;
                match result {
                    Ok(Ok(canonical)) => {
                        page.imp().title_override.set_text(&canonical);
                        page.toast_in_window(&format!("Radarr: matched \"{canonical}\""));
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Radarr lookup failed: {e:#}");
                        page.toast_in_window(&format!("Radarr lookup failed: {e}"));
                    }
                    Err(e) => {
                        tracing::error!("Radarr channel closed: {e}");
                    }
                }
            }
        ));
    }

    fn apply_episode_titles(&self, pairs: Vec<(u32, String)>) {
        // Fill in our episode_entries in order. Episode numbers in
        // `pairs` are absolute (per-season); we assume the selected
        // titles map sequentially to episode numbers starting at the
        // first pair's episode_number. For now we just iterate entries
        // in order and pull off pairs as we go.
        let entries = self.imp().episode_entries.borrow();
        for (entry, (_, title)) in entries.iter().zip(pairs.into_iter()) {
            entry.set_text(&title);
        }
    }

    fn toast_in_window(&self, message: &str) {
        if let Some(window) = self.parent_window() {
            let toast = adw::Toast::builder().title(message).timeout(4).build();
            window.add_toast(toast);
        }
    }

    fn start_conversion(&self, format: OutputFormat) {
        let Some(input) = self.imp().iso_path.borrow().clone() else {
            tracing::error!("convert requested but no input path on TitleListPage");
            return;
        };
        let Some(source) = *self.imp().source_kind.borrow() else {
            self.parent_window().map(|w| w.add_toast(
                adw::Toast::builder()
                    .title("No 3D content detected — nothing to convert.")
                    .timeout(4)
                    .build(),
            ));
            return;
        };

        let output = ConversionPlan::default_output_path(&input, format);
        let plan = ConversionPlan {
            input,
            output,
            format,
            source,
            codec: self.selected_codec(),
            hw_backend: self.selected_hw_backend(),
        };

        let (tx, rx) = async_channel::bounded::<anyhow::Result<PathBuf>>(1);
        let plan_for_task = plan.clone();
        crate::runtime::tokio_runtime().spawn(async move {
            let _ = tx.send(run_conversion(plan_for_task, None).await).await;
        });

        let label = format.label().to_string();
        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = page)]
            self,
            async move {
                let result = rx.recv().await;
                let Some(window) = page.parent_window() else { return; };
                match result {
                    Ok(Ok(output_path)) => {
                        let toast = adw::Toast::builder()
                            .title(&format!(
                                "Converted to {label} → {}",
                                output_path.display()
                            ))
                            .timeout(6)
                            .build();
                        window.add_toast(toast);
                    }
                    Ok(Err(e)) => {
                        tracing::error!("conversion failed: {e:#}");
                        let toast = adw::Toast::builder()
                            .title(&format!("Conversion failed: {e}"))
                            .timeout(8)
                            .build();
                        window.add_toast(toast);
                    }
                    Err(e) => {
                        tracing::error!("conversion channel closed: {e}");
                    }
                }
            }
        ));
    }

    fn parent_window(&self) -> Option<adw::ToastOverlay> {
        let mut next: Option<gtk::Widget> = self.parent();
        while let Some(widget) = next {
            if let Ok(overlay) = widget.clone().downcast::<adw::ToastOverlay>() {
                return Some(overlay);
            }
            next = widget.parent();
        }
        None
    }

    fn collect_episode_titles(&self) -> HashMap<u32, String> {
        let titles = self.imp().titles.borrow();
        self.imp()
            .episode_entries
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let text = entry.text();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    titles.get(i).map(|t| (t.index, trimmed.to_string()))
                }
            })
            .collect()
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
        let source = match self.imp().source.borrow().clone() {
            Some(s) => s,
            None => {
                tracing::error!("rip requested but source not set on TitleListPage");
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
        let identities_snapshot = self.imp().identities.borrow().clone();
        let identification_for_plan = build_pseudo_identification(
            &titles_snapshot,
            &disc_name,
            identities_snapshot,
        );

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
        // First TheDiscDB match (if any) supplies year + external IDs.
        // For the human-facing title we prefer the user's edit, falling
        // back to the matched item title and then the disc label.
        let primary_identity = identification_for_plan.identities.first().cloned();
        let user_title = self.imp().title_override.text().to_string();
        let chosen_title = if !user_title.trim().is_empty() {
            user_title
        } else {
            primary_identity
                .as_ref()
                .map(|i| i.item_title.clone())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| disc_name.clone())
        };
        let chosen_season = self.imp().season_override.value().max(0.0) as u32;
        let mut naming_opts = naming_opts_for_unidentified(
            library_root.clone(),
            scheme,
            content_kind,
            &chosen_title,
        );
        if let Some(identity) = primary_identity.as_ref() {
            naming_opts.disc_year = identity.year;
            naming_opts.tmdb_id = identity.tmdb_id;
            naming_opts.imdb_id = identity.imdb_id.clone();
        }
        if content_kind == DiscContentKind::Series {
            naming_opts.season = chosen_season.max(1);
        }
        // 3D output format is now per-title (built below and passed to
        // plan_rip); the encoder backend is a global preference.
        naming_opts.conversion_hw_backend = self.selected_hw_backend();
        // Output codec is a global preference (Preferences → Encoding).
        naming_opts.conversion_codec = self.selected_codec();
        let episode_titles = self.collect_episode_titles();
        // Per-title overrides from the TitleDetailPage: display title
        // → filename + segment.title; role → naming-scheme bucket
        // (Main vs Extras subfolder).
        let title_edits = self.imp().title_edits.borrow();
        let display_overrides: HashMap<u32, String> = title_edits
            .iter()
            .filter_map(|(idx, e)| {
                e.display_title
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| (*idx, s.clone()))
            })
            .collect();
        let role_overrides: HashMap<u32, crate::identify::TitleRole> = title_edits
            .iter()
            .filter_map(|(idx, e)| e.role.map(|r| (*idx, r)))
            .collect();
        drop(title_edits);
        let format_by_index = self.format_by_index();
        let plan = plan_rip(
            &identification_for_plan,
            &selected,
            Some(&naming_opts),
            &episode_titles,
            &display_overrides,
            &role_overrides,
            &format_by_index,
        );
        let mut queue: Vec<RipQueueItem> = plan.into_iter().map(RipQueueItem::from).collect();
        // For an unencrypted 3D Blu-ray, let the orchestrator decode the feature
        // straight off the disc (SSIF) instead of a makemkvcon rip + convert.
        let iso_path = self.imp().iso_path.borrow().clone();
        let ssif_note =
            attach_ssif_feature(&mut queue, &source, &identification_for_plan, iso_path.as_deref());

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
        if let Some(note) = ssif_note {
            progress.append_log(&note);
        }

        if let Some(nav) = navigation_view(self) {
            nav.push(&progress);
        } else {
            tracing::warn!("TitleListPage has no NavigationView ancestor; cannot push RipProgressPage");
        }

        crate::rip::orchestrator::run_rip_queue(source, queue, progress.downgrade());
    }
}

/// For an unencrypted 3D-Blu-ray `Folder` source, attach the feature's SSIF
/// clips to the queue item whose title IS the feature — matched by duration, so
/// it can't pick the wrong title. The orchestrator then decodes that item's SSIF
/// directly (no makemkvcon rip, no intermediate MKV). No-op for anything else
/// (physical/encrypted discs, non-3D, no matching title) → the makemkvcon path.
fn attach_ssif_feature(
    queue: &mut [RipQueueItem],
    source: &crate::rip::makemkv::ScanSource,
    ident: &IdentificationResult,
    iso_path: Option<&std::path::Path>,
) -> Option<String> {
    use crate::rip::bd_playlist::{
        find_feature_3d, find_feature_3d_iso, is_encrypted, is_encrypted_iso, Feature, FeatureClips,
    };
    use crate::rip::makemkv::ScanSource;
    use crate::rip::udf::Udf;

    // Resolve the 3D feature + where its clips live. Prefer reading the ISO
    // directly via the UDF reader — that works even inside a Flatpak sandbox,
    // where the host-side /run/media loop mount isn't visible to us. Only fall
    // back to walking a readable mount. Each branch either returns a note early
    // or yields (feature, clip source).
    let is_iso = |p: &std::path::Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso"));
    let (feature, clips): (Feature, FeatureClips) = if let Some(iso) = iso_path.filter(|p| is_iso(p)) {
        let mut f = match std::fs::File::open(iso) {
            Ok(f) => f,
            Err(e) => return Some(format!("3D fast path unavailable: couldn't open the ISO ({e}) — ripping via MakeMKV.")),
        };
        let udf = match Udf::open(&mut f) {
            Ok(u) => u,
            Err(e) => return Some(format!("3D fast path unavailable: couldn't read the ISO's filesystem ({e}) — ripping via MakeMKV.")),
        };
        if is_encrypted_iso(&udf, &mut f) {
            return Some("3D fast path unavailable: disc is AACS-encrypted — ripping via MakeMKV.".into());
        }
        match find_feature_3d_iso(&udf, &mut f) {
            Some(feat) => {
                let clips = FeatureClips::Iso { iso: iso.to_path_buf(), clips: feat.clips.clone() };
                (feat, clips)
            }
            None => return Some("3D fast path unavailable: no MVC feature playlist in the ISO — ripping via MakeMKV.".into()),
        }
    } else if let ScanSource::Folder(mount) = source {
        if is_encrypted(mount) {
            return Some("3D fast path unavailable: disc is AACS-encrypted — ripping via MakeMKV.".into());
        }
        match find_feature_3d(mount) {
            Some(feat) => {
                let clips = FeatureClips::Mount(feat.clip_paths(mount));
                (feat, clips)
            }
            None => {
                let readable = std::fs::read_dir(mount.join("BDMV")).is_ok();
                return Some(if readable {
                    format!("3D fast path unavailable: no MVC feature playlist under {} — ripping via MakeMKV.", mount.display())
                } else {
                    format!(
                        "3D fast path unavailable: can't read {} from inside the sandbox \
                         (mount not propagated) — ripping via MakeMKV instead.",
                        mount.display()
                    )
                });
            }
        }
    } else {
        // Physical disc / MKV re-identify: MakeMKV path, nothing to report.
        return None;
    };

    let feat_secs = feature.duration_seconds();
    let n_clips = feature.clips.len();
    let via = match &clips {
        FeatureClips::Iso { .. } => "straight from the ISO",
        FeatureClips::Mount(_) => "straight from the disc",
    };
    for item in queue.iter_mut() {
        if item.conversion_format.is_none() {
            continue;
        }
        let Some(t) = ident.scan.titles.iter().find(|t| t.index == item.title_index) else { continue };
        // The makemkvcon title and the .mpls feature are the same movie, so
        // their runtimes match closely; require within ~5 % (min 3 s).
        let tol = (feat_secs / 20).max(3);
        if t.has_mvc_stream() && feat_secs.abs_diff(t.duration_seconds.unwrap_or(0)) <= tol {
            item.ssif_clips = Some(clips.clone());
            return Some(format!(
                "3D fast path: decoding {n_clips} SSIF clip(s) {via} — no MakeMKV rip, no intermediate MKV."
            ));
        }
    }
    Some(format!(
        "3D fast path unavailable: found a {}-min MVC feature but no queued title matched its runtime — ripping via MakeMKV.",
        feat_secs / 60
    ))
}

fn build_pseudo_identification(
    titles: &[TitleAttributes],
    disc_name: &str,
    identities: Vec<Identity>,
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
        identities,
        lookup_status: crate::identify::pipeline::LookupStatus::NotAttempted,
        source: crate::rip::makemkv::ScanSource::Iso(std::path::PathBuf::new()),
        source_file: None,
        has_mvc: false,
        bdmt: None,
        dvd_region_code: None,
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

fn role_badge(role: TitleRole) -> &'static str {
    match role {
        TitleRole::Main => "Main feature",
        TitleRole::Trailer => "Trailer",
        TitleRole::BehindTheScenes => "Behind the scenes",
        TitleRole::DeletedScene => "Deleted scene",
        TitleRole::Featurette => "Featurette",
        TitleRole::Interview => "Interview",
        TitleRole::Scene => "Scene",
        TitleRole::Short => "Short",
        TitleRole::Other => "Extra",
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
