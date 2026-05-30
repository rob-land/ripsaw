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
        #[template_child] pub series_toggle: TemplateChild<adw::SwitchRow>,
        #[template_child] pub title_override: TemplateChild<adw::EntryRow>,
        #[template_child] pub season_override: TemplateChild<adw::SpinRow>,
        #[template_child] pub output_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub output_format_row: TemplateChild<adw::ComboRow>,

        pub checkboxes: RefCell<Vec<gtk::CheckButton>>,
        pub episode_entries: RefCell<Vec<gtk::Entry>>,
        pub titles: RefCell<Vec<TitleAttributes>>,
        pub iso_path: RefCell<Option<PathBuf>>,
        pub source: RefCell<Option<crate::rip::makemkv::ScanSource>>,
        pub disc_name: RefCell<Option<String>>,
        pub source_kind: RefCell<Option<StereoSource>>,
        pub identities: RefCell<Vec<Identity>>,
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
                    let mut r = crate::mvc::ebml::EbmlReader::new(f);
                    matches!(crate::mvc::mvcc::find_mvcc_bytes(&mut r), Ok(Some(_)))
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
        // Show the 3D output options only when we detected MVC.
        page.imp().output_group.set_visible(result.has_mvc);
        // Default the format combo to "Off" so a regular rip is a single
        // click; user opts in by changing it.
        page.imp().output_format_row.set_selected(0);
        page
    }

    fn populate_with_identity(&self, result: &IdentificationResult) {
        let group = self.imp().title_group.get();
        group.set_title(&format_group_title(result));
        group.set_description(Some(&format_group_description(result)));
        self.imp().identities.replace(result.identities.clone());
        if let Some(name) = &result.scan.disc.name {
            self.set_title(name);
        }
        let detected = auto_detect_content_kind(&result.scan.titles);
        self.imp().series_toggle.set_active(detected == DiscContentKind::Series);

        // Pre-fill the editable title and season fields from the disc label.
        if let Some(disc_name) = &result.scan.disc.name {
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
        let mut checkboxes = Vec::with_capacity(scan.titles.len());
        let mut episode_entries = Vec::with_capacity(scan.titles.len());

        let pairs: Vec<(u32, &str)> = scan
            .titles
            .iter()
            .map(|t| (t.index, t.segment_map.as_deref().unwrap_or("")))
            .collect();
        let relations = analyze_relations(&pairs);

        // First identity (highest-confidence TheDiscDB match) provides
        // per-title roles + display titles. Match by sourceFile, not
        // index -- TheDiscDB's index is its own ordering and does not
        // line up with MakeMKV's title index.
        let identities = self.imp().identities.borrow();
        let identity_titles: &[TitleIdentity] = identities
            .first()
            .map(|i| i.titles.as_slice())
            .unwrap_or(&[]);

        for (t, relation) in scan.titles.iter().zip(relations.iter()) {
            let identity = crate::rip::plan::match_identity_for(identity_titles, t);
            let role = identity.map(|i| i.role);
            let display_title = identity
                .map(|i| i.display_title.as_str())
                .filter(|s| !s.is_empty());

            // Row title: TheDiscDB display title if we have one, otherwise
            // the MakeMKV name, otherwise just "Title N".
            let title_label = match (display_title, t.name.as_deref()) {
                (Some(dt), _) => format!("Title {} — {}", t.index, dt),
                (None, Some(n)) if !n.is_empty() => format!("Title {} — {}", t.index, n),
                (None, _) => format!("Title {}", t.index),
            };
            let duration = format_duration(t.duration_seconds.unwrap_or(0));
            let size = format_bytes(t.size_bytes.unwrap_or(0));
            let source = t.source_file.as_deref().unwrap_or("?");

            let mut subtitle_parts: Vec<String> = Vec::with_capacity(5);
            if let Some(r) = role {
                subtitle_parts.push(role_badge(r).to_string());
            }
            subtitle_parts.push(duration);
            subtitle_parts.push(size);
            subtitle_parts.push(source.to_string());
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

            let episode_entry = gtk::Entry::builder()
                .placeholder_text("Episode title (optional)")
                .valign(gtk::Align::Center)
                .width_chars(28)
                .visible(false)
                .build();
            row.add_suffix(&episode_entry);

            group.add(&row);
            checkboxes.push(check);
            episode_entries.push(episode_entry);
        }

        self.imp().checkboxes.replace(checkboxes);
        self.imp().episode_entries.replace(episode_entries);
        self.refresh_rip_sensitivity();
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

        let group = gio::SimpleActionGroup::new();
        group.add_action(&process_action);
        group.add_action(&sonarr_action);
        self.insert_action_group("page", Some(&group));
    }

    /// Map the output-format ComboRow selection to an OutputFormat.
    /// Returns `None` when the user picked "Off" (or the row is hidden).
    fn selected_output_format(&self) -> Option<OutputFormat> {
        // Row order must mirror the StringList in title-list-page.blp.
        match self.imp().output_format_row.selected() {
            1 => Some(OutputFormat::FullSbs),
            2 => Some(OutputFormat::HalfSbs),
            3 => Some(OutputFormat::FullTab),
            4 => Some(OutputFormat::HalfTab),
            5 => Some(OutputFormat::FrameSequential),
            _ => None,
        }
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
        let format = self.selected_output_format();

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

        // Physical disc / ISO: run the rip pipeline. The selected
        // conversion format is recorded in the warning below for now;
        // chaining "rip then convert" in a single Process click is
        // tracked separately and will fold into the same start_rip
        // path once the orchestrator gains a post-rip convert hook.
        if format.is_some() {
            if let Some(window) = self.parent_window() {
                window.add_toast(
                    adw::Toast::builder()
                        .title("Ripping first; re-open the produced MKV to convert it for now.")
                        .timeout(6)
                        .build(),
                );
            }
        }
        self.start_rip();
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
        let plan = ConversionPlan { input, output, format, source };

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
        let episode_titles = self.collect_episode_titles();
        let plan = plan_rip(
            &identification_for_plan,
            &selected,
            Some(&naming_opts),
            &episode_titles,
        );
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

        crate::rip::orchestrator::run_rip_queue(source, queue, progress.downgrade());
    }
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
        source: crate::rip::makemkv::ScanSource::Iso(std::path::PathBuf::new()),
        source_file: None,
        has_mvc: false,
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
