use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, clone};
use gtk::{gio, CompositeTemplate};

use crate::settings::{settings, SchemeKind};

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/land/rob/ripsaw/ui/preferences-dialog.ui")]
    pub struct PreferencesDialog {
        #[template_child] pub library_root_row: TemplateChild<adw::ActionRow>,
        #[template_child] pub scheme_combo: TemplateChild<adw::ComboRow>,
        #[template_child] pub codec_combo: TemplateChild<adw::ComboRow>,
        #[template_child] pub encoder_backend_combo: TemplateChild<adw::ComboRow>,
        #[template_child] pub sonarr_url_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub sonarr_key_row: TemplateChild<adw::PasswordEntryRow>,
        #[template_child] pub radarr_url_row: TemplateChild<adw::EntryRow>,
        #[template_child] pub radarr_key_row: TemplateChild<adw::PasswordEntryRow>,
        #[template_child] pub tmdb_key_row: TemplateChild<adw::PasswordEntryRow>,
        #[template_child] pub catalogue_status_row: TemplateChild<adw::ActionRow>,
        #[template_child] pub catalogue_sync_button: TemplateChild<gtk::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PreferencesDialog {
        const NAME: &'static str = "RipsawPreferencesDialog";
        type Type = super::PreferencesDialog;
        type ParentType = adw::PreferencesDialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PreferencesDialog {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().load_current();
            self.obj().connect_signals();
        }
    }
    impl WidgetImpl for PreferencesDialog {}
    impl AdwDialogImpl for PreferencesDialog {}
    impl PreferencesDialogImpl for PreferencesDialog {}
}

glib::wrapper! {
    pub struct PreferencesDialog(ObjectSubclass<imp::PreferencesDialog>)
        @extends gtk::Widget, adw::Dialog, adw::PreferencesDialog,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for PreferencesDialog {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl PreferencesDialog {
    fn load_current(&self) {
        let current = settings().lock().expect("settings mutex").clone();
        self.imp().library_root_row.set_subtitle(&format_root(&current.library_root));
        self.imp().scheme_combo.set_selected(current.scheme.to_index());
        self.imp()
            .codec_combo
            .set_selected(current.conversion_codec().to_ui_index());
        self.imp()
            .encoder_backend_combo
            .set_selected(current.conversion_hw_backend().to_ui_index());
        self.imp().sonarr_url_row.set_text(current.sonarr.url.as_deref().unwrap_or(""));
        self.imp().sonarr_key_row.set_text(current.sonarr.api_key.as_deref().unwrap_or(""));
        self.imp().radarr_url_row.set_text(current.radarr.url.as_deref().unwrap_or(""));
        self.imp().radarr_key_row.set_text(current.radarr.api_key.as_deref().unwrap_or(""));
        self.imp().tmdb_key_row.set_text(current.tmdb_api_key.as_deref().unwrap_or(""));
        self.refresh_catalogue_status();
    }

    /// Update the "Local catalogue" row from the mirror on disk and set
    /// the button label to Download (absent) or Refresh (present).
    fn refresh_catalogue_status(&self) {
        let root = crate::settings::thediscdb_mirror_root();
        let count = crate::identify::thediscdb_local::disc_count(&root);
        if count > 0 {
            self.imp()
                .catalogue_status_row
                .set_subtitle(&format!("{count} discs • {}", root.display()));
            self.imp().catalogue_sync_button.set_label("Refresh");
        } else {
            self.imp()
                .catalogue_status_row
                .set_subtitle("Not downloaded — identify will use the (currently unreliable) website");
            self.imp().catalogue_sync_button.set_label("Download");
        }
    }

    /// Sync the mirror on a worker thread, with a busy button + toast.
    fn start_catalogue_sync(&self) {
        let button = self.imp().catalogue_sync_button.get();
        button.set_sensitive(false);
        button.set_label("Downloading…");
        self.imp()
            .catalogue_status_row
            .set_subtitle("Syncing the JSON catalogue from GitHub… (~350 MB, may take a few minutes)");

        let root = crate::settings::thediscdb_mirror_root();
        let (tx, rx) = async_channel::bounded::<anyhow::Result<usize>>(1);
        crate::runtime::tokio_runtime().spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::identify::thediscdb_local::sync_mirror(&root)
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("sync task panicked: {e}")));
            let _ = tx.send(result).await;
        });
        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = dialog)]
            self,
            async move {
                let outcome = rx.recv().await;
                dialog.imp().catalogue_sync_button.set_sensitive(true);
                match outcome {
                    Ok(Ok(count)) => {
                        dialog.toast(&format!("Disc catalogue ready — {count} discs."));
                    }
                    Ok(Err(e)) => {
                        tracing::error!("catalogue sync failed: {e:#}");
                        dialog.toast(&format!("Catalogue sync failed: {e}"));
                    }
                    Err(_) => {}
                }
                dialog.refresh_catalogue_status();
            }
        ));
    }

    fn toast(&self, text: &str) {
        self.add_toast(adw::Toast::builder().title(text).timeout(6).build());
    }

    fn connect_signals(&self) {
        self.imp().catalogue_sync_button.connect_clicked(clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.start_catalogue_sync()
        ));

        // Activating the row opens a folder chooser.
        self.imp().library_root_row.connect_activated(clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.pick_library_root()
        ));

        // Combo selection changes -> persist the new scheme.
        self.imp().scheme_combo.connect_selected_notify(clone!(
            #[weak(rename_to = dialog)]
            self,
            move |combo| {
                let scheme = SchemeKind::from_index(combo.selected());
                let mut guard = settings().lock().expect("settings mutex");
                if guard.scheme != scheme {
                    guard.scheme = scheme;
                    if let Err(e) = guard.save() {
                        tracing::warn!("failed to save scheme preference: {e}");
                    }
                }
                let _ = dialog; // keep weak alive for the closure
            }
        ));

        // Codec -> persist the chosen output codec.
        self.imp().codec_combo.connect_selected_notify(clone!(
            #[weak(rename_to = dialog)]
            self,
            move |combo| {
                let codec = crate::convert::hw::EncodeCodec::from_ui_index(combo.selected());
                let mut guard = settings().lock().expect("settings mutex");
                if guard.conversion_codec != Some(codec) {
                    guard.conversion_codec = Some(codec);
                    if let Err(e) = guard.save() {
                        tracing::warn!("failed to save codec preference: {e}");
                    }
                }
                let _ = dialog;
            }
        ));

        // Encoder backend -> persist the chosen HW backend.
        self.imp().encoder_backend_combo.connect_selected_notify(clone!(
            #[weak(rename_to = dialog)]
            self,
            move |combo| {
                let backend =
                    crate::convert::hw::HwBackend::from_ui_index(combo.selected());
                let mut guard = settings().lock().expect("settings mutex");
                if guard.conversion_hw_backend != Some(backend) {
                    guard.conversion_hw_backend = Some(backend);
                    if let Err(e) = guard.save() {
                        tracing::warn!("failed to save encoder backend preference: {e}");
                    }
                }
                let _ = dialog;
            }
        ));

        // Sonarr / Radarr fields: persist on focus-out / apply via the
        // EntryRow's `apply` signal.
        let imp = self.imp();
        connect_entry_apply(&imp.sonarr_url_row, |s, t| s.sonarr.url = nonempty(t));
        connect_password_apply(&imp.sonarr_key_row, |s, t| s.sonarr.api_key = nonempty(t));
        connect_entry_apply(&imp.radarr_url_row, |s, t| s.radarr.url = nonempty(t));
        connect_password_apply(&imp.radarr_key_row, |s, t| s.radarr.api_key = nonempty(t));
        connect_password_apply(&imp.tmdb_key_row, |s, t| s.tmdb_api_key = nonempty(t));
    }

    fn pick_library_root(&self) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose your library root")
            .modal(true)
            .build();
        let parent_window: Option<gtk::Window> = self
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok());

        dialog.select_folder(
            parent_window.as_ref(),
            gio::Cancellable::NONE,
            clone!(
                #[weak(rename_to = pref)]
                self,
                move |result| {
                    match result {
                        Ok(file) => {
                            if let Some(path) = file.path() {
                                pref.set_library_root(path);
                            }
                        }
                        Err(e) if e.matches(gtk::DialogError::Dismissed) => {}
                        Err(e) => tracing::warn!("folder dialog error: {e}"),
                    }
                }
            ),
        );
    }

    fn set_library_root(&self, path: PathBuf) {
        let mut guard = settings().lock().expect("settings mutex");
        guard.library_root = Some(path.clone());
        if let Err(e) = guard.save() {
            tracing::warn!("failed to save library_root: {e}");
        }
        drop(guard);
        self.imp().library_root_row.set_subtitle(&format_root(&Some(path)));
    }
}

fn format_root(p: &Option<PathBuf>) -> String {
    match p {
        Some(path) => path.display().to_string(),
        None => "not configured (rips will use ~/Videos)".into(),
    }
}

fn nonempty(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Bind an AdwEntryRow so any text change immediately persists to
/// disk. We previously used `connect_apply`, which only fires when
/// the user presses Enter or clicks an apply-icon -- and we never
/// surface that icon -- so users who typed an API key and just
/// closed the dialog saw their input silently dropped.
fn connect_entry_apply(
    row: &adw::EntryRow,
    update: impl Fn(&mut crate::settings::UserSettings, &str) + 'static,
) {
    row.connect_changed(move |row| {
        let text = row.text().to_string();
        let mut guard = settings().lock().expect("settings mutex");
        update(&mut guard, &text);
        if let Err(e) = guard.save() {
            tracing::warn!("failed to save settings: {e}");
        }
    });
}

fn connect_password_apply(
    row: &adw::PasswordEntryRow,
    update: impl Fn(&mut crate::settings::UserSettings, &str) + 'static,
) {
    row.connect_changed(move |row| {
        let text = row.text().to_string();
        let mut guard = settings().lock().expect("settings mutex");
        update(&mut guard, &text);
        if let Err(e) = guard.save() {
            tracing::warn!("failed to save settings: {e}");
        }
    });
}
