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
        self.setup_drop_target();
    }

    fn setup_drop_target(&self) {
        // Accept GFile (and the URI-list fallback) anywhere on the window.
        let target = gtk::DropTarget::new(gio::File::static_type(), gtk::gdk::DragAction::COPY);
        target.set_types(&[gio::File::static_type(), glib::types::Type::STRING]);
        target.connect_drop(clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            false,
            move |_target, value, _x, _y| {
                let path = path_from_drop_value(value);
                match path {
                    Some(p) if is_iso_like(&p) => {
                        window.scan_iso(p);
                        true
                    }
                    Some(p) => {
                        window.toast(&format!(
                            "Not an ISO file: {}",
                            p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                        ));
                        false
                    }
                    None => false,
                }
            }
        ));
        self.add_controller(target);
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

fn path_from_drop_value(value: &gtk::glib::Value) -> Option<PathBuf> {
    if let Ok(file) = value.get::<gio::File>() {
        return file.path();
    }
    if let Ok(text) = value.get::<&str>() {
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(path) = trimmed.strip_prefix("file://") {
                return Some(PathBuf::from(percent_decode(path)));
            }
            return Some(PathBuf::from(trimmed));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hi = chars.next();
            let lo = chars.next();
            match (hi, lo) {
                (Some(h), Some(l)) => {
                    if let Ok(byte) = u8::from_str_radix(&format!("{h}{l}"), 16) {
                        out.push(byte as char);
                        continue;
                    }
                    out.push('%');
                    out.push(h);
                    out.push(l);
                }
                _ => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_iso_like(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("iso"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_space() {
        assert_eq!(percent_decode("Jurassic%20Park%20(1993).iso"), "Jurassic Park (1993).iso");
    }

    #[test]
    fn percent_decode_passes_through_invalid_escape() {
        assert_eq!(percent_decode("a%2Zb"), "a%2Zb");
    }

    #[test]
    fn is_iso_like_recognises_iso_case_insensitive() {
        use std::path::Path;
        assert!(is_iso_like(Path::new("foo.iso")));
        assert!(is_iso_like(Path::new("FOO.ISO")));
        assert!(is_iso_like(Path::new("/abs/path/foo.iso")));
        assert!(!is_iso_like(Path::new("foo.mkv")));
        assert!(!is_iso_like(Path::new("foo")));
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
