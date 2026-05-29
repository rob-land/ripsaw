use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, clone};
use gtk::{gio, CompositeTemplate};

use crate::identify::pipeline::{identify_iso, IdentificationResult};
use crate::ui::title_list_page::TitleListPage;

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/dev/threedrip/ThreeDrip/ui/window.ui")]
    pub struct ThreeDripWindow {
        #[template_child]
        pub toasts: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub nav: TemplateChild<adw::NavigationView>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ThreeDripWindow {
        const NAME: &'static str = "ThreeDripWindow";
        type Type = super::ThreeDripWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ThreeDripWindow {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_actions();
        }
    }
    impl WidgetImpl for ThreeDripWindow {}
    impl WindowImpl for ThreeDripWindow {}
    impl ApplicationWindowImpl for ThreeDripWindow {}
    impl AdwApplicationWindowImpl for ThreeDripWindow {}
}

glib::wrapper! {
    pub struct ThreeDripWindow(ObjectSubclass<imp::ThreeDripWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
                    gtk::Root, gtk::ShortcutManager,
                    gio::ActionGroup, gio::ActionMap;
}

impl ThreeDripWindow {
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    fn setup_actions(&self) {
        let open_iso = gio::ActionEntry::builder("open-iso")
            .activate(|window: &Self, _action, _param| {
                window.open_iso();
            })
            .build();
        self.add_action_entries([open_iso]);
    }

    fn open_iso(&self) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose disc or ISO")
            .modal(true)
            .build();

        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Disc images and folders"));
        filter.add_pattern("*.iso");
        filter.add_pattern("*.ISO");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));

        dialog.open(
            Some(self),
            gio::Cancellable::NONE,
            clone!(
                #[weak(rename_to = window)]
                self,
                move |result| {
                    match result {
                        Ok(file) => {
                            if let Some(path) = file.path() {
                                window.scan_iso(path);
                            } else {
                                tracing::warn!("file dialog returned a file with no path");
                            }
                        }
                        Err(e) if e.matches(gtk::DialogError::Dismissed) => {
                            tracing::debug!("file dialog dismissed by user");
                        }
                        Err(e) => {
                            tracing::warn!("file dialog error: {e}");
                        }
                    }
                }
            ),
        );
    }

    fn scan_iso(&self, path: PathBuf) {
        tracing::info!("identifying {}", path.display());
        self.toast(&format!("Scanning {}...", path.display()));

        let (tx, rx) = async_channel::bounded(1);
        let path_for_task = path.clone();
        crate::runtime::tokio_runtime().spawn(async move {
            let _ = tx.send(identify_iso(path_for_task).await).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = window)]
            self,
            async move {
                match rx.recv().await {
                    Ok(Ok(result)) => window.show_identification(result),
                    Ok(Err(e)) => {
                        tracing::error!("identify failed: {e:#}");
                        window.toast(&format!("Identify failed: {e}"));
                    }
                    Err(e) => {
                        tracing::error!("scan channel closed: {e}");
                        window.toast("Scan worker stopped unexpectedly");
                    }
                }
            }
        ));
    }

    fn show_identification(&self, result: IdentificationResult) {
        let n = result.scan.titles.len();
        let disc_name = result
            .scan
            .disc
            .name
            .as_deref()
            .unwrap_or("(unnamed disc)");

        if result.is_identified() {
            let slug = result
                .identities
                .first()
                .map(|i| i.release_slug.as_str())
                .unwrap_or("");
            self.toast(&format!("{disc_name}: identified as {slug}"));
        } else if let Some(h) = &result.content_hash {
            self.toast(&format!(
                "{disc_name}: {n} titles • {} • not in catalog (hash {})",
                describe_disc_type(result.disc_type),
                &h[..12]
            ));
        } else {
            self.toast(&format!(
                "{disc_name}: {n} titles • {} • not mounted (no hash, no lookup)",
                describe_disc_type(result.disc_type)
            ));
        }

        let iso_path = result
            .mount
            .as_ref()
            .map(|m| m.iso_path.clone())
            .unwrap_or_else(|| PathBuf::from(""));
        let page = TitleListPage::from_identification(&result, iso_path);
        self.imp().nav.push(&page);

        // Hand the live MountedIso off to the application-wide tracker so it
        // gets cleaned up on shutdown rather than leaking until reboot.
        if let Some(mount) = result.mount {
            crate::state::track_mount(mount);
        }
    }

    fn toast(&self, message: &str) {
        let toast = adw::Toast::builder().title(message).timeout(4).build();
        self.imp().toasts.add_toast(toast);
    }
}

fn describe_disc_type(t: crate::identify::DiscType) -> &'static str {
    match t {
        crate::identify::DiscType::Dvd => "DVD",
        crate::identify::DiscType::BluRay => "Blu-ray",
        crate::identify::DiscType::UltraHdBluRay => "4K UHD",
        crate::identify::DiscType::BluRay3D => "3D Blu-ray",
    }
}
