use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, clone};
use gtk::{gio, CompositeTemplate};

use crate::settings::{settings, SchemeKind};

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/dev/threedrip/ThreeDrip/ui/preferences-dialog.ui")]
    pub struct PreferencesDialog {
        #[template_child] pub library_root_row: TemplateChild<adw::ActionRow>,
        #[template_child] pub scheme_combo: TemplateChild<adw::ComboRow>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PreferencesDialog {
        const NAME: &'static str = "ThreeDripPreferencesDialog";
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
    }

    fn connect_signals(&self) {
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
