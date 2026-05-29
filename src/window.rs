use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, clone};
use gtk::{gio, CompositeTemplate};

use crate::identify::pipeline::{
    identify_iso, identify_mkv, identify_physical_disc, IdentificationResult,
};
use crate::rip::drive::{detect_mounted_optical_discs, DetectedDisc};
use crate::ui::title_list_page::TitleListPage;

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/land/rob/ripsaw/ui/window.ui")]
    pub struct RipsawWindow {
        #[template_child]
        pub toasts: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub nav: TemplateChild<adw::NavigationView>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RipsawWindow {
        const NAME: &'static str = "RipsawWindow";
        type Type = super::RipsawWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for RipsawWindow {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_actions();
        }
    }
    impl WidgetImpl for RipsawWindow {}
    impl WindowImpl for RipsawWindow {}
    impl ApplicationWindowImpl for RipsawWindow {}
    impl AdwApplicationWindowImpl for RipsawWindow {}
}

glib::wrapper! {
    pub struct RipsawWindow(ObjectSubclass<imp::RipsawWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
                    gtk::Root, gtk::ShortcutManager,
                    gio::ActionGroup, gio::ActionMap;
}

impl RipsawWindow {
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    fn setup_actions(&self) {
        let open_iso = gio::ActionEntry::builder("open-iso")
            .activate(|window: &Self, _action, _param| {
                window.open_iso();
            })
            .build();
        let open_disc = gio::ActionEntry::builder("open-disc")
            .activate(|window: &Self, _action, _param| {
                window.open_disc();
            })
            .build();
        self.add_action_entries([open_iso, open_disc]);
        self.setup_drop_target();
    }

    fn open_disc(&self) {
        let discs = match detect_mounted_optical_discs() {
            Ok(d) if d.is_empty() => {
                self.toast("No optical disc detected. Insert one and try again.");
                return;
            }
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("optical-drive detection failed: {e}");
                self.toast(&format!("Could not detect optical drives: {e}"));
                return;
            }
        };
        // For now, just pick the first mounted disc. A picker UI for
        // multi-drive systems can come later.
        let disc = discs.into_iter().next().expect("non-empty discs");
        self.scan_physical_disc(disc);
    }

    fn scan_physical_disc(&self, disc: DetectedDisc) {
        tracing::info!(
            "identifying physical disc {} at {}",
            disc.device.display(),
            disc.mount_path.display(),
        );
        let label = disc.label.clone().unwrap_or_else(|| "disc".to_string());
        let scanning_page = self.push_scanning_page(&format!(
            "Scanning {label}…",
        ));

        let (tx, rx) = async_channel::bounded(1);
        let index = disc.disc_index;
        let mount = disc.mount_path.clone();
        crate::runtime::tokio_runtime().spawn(async move {
            let _ = tx.send(identify_physical_disc(index, mount).await).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = window)]
            self,
            async move {
                let result = rx.recv().await;
                window.dismiss_scanning_page(&scanning_page);
                match result {
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
                    Some(p) if !matches!(input_kind(&p), InputKind::Unknown) => {
                        window.dispatch_input(p);
                        true
                    }
                    Some(p) => {
                        window.toast(&format!(
                            "Not an ISO or MKV file: {}",
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
            .title("Choose a disc image or MKV file")
            .modal(true)
            .build();

        let iso_filter = gtk::FileFilter::new();
        iso_filter.set_name(Some("Disc images (.iso)"));
        iso_filter.add_pattern("*.iso");
        iso_filter.add_pattern("*.ISO");

        let mkv_filter = gtk::FileFilter::new();
        mkv_filter.set_name(Some("Matroska video (.mkv)"));
        mkv_filter.add_pattern("*.mkv");
        mkv_filter.add_pattern("*.MKV");

        let both_filter = gtk::FileFilter::new();
        both_filter.set_name(Some("All supported inputs"));
        both_filter.add_pattern("*.iso");
        both_filter.add_pattern("*.ISO");
        both_filter.add_pattern("*.mkv");
        both_filter.add_pattern("*.MKV");

        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&both_filter);
        filters.append(&iso_filter);
        filters.append(&mkv_filter);
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&both_filter));

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
                                window.dispatch_input(path);
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

    fn dispatch_input(&self, path: PathBuf) {
        match input_kind(&path) {
            InputKind::Mkv => self.scan_mkv(path),
            InputKind::Iso => self.scan_iso(path),
            InputKind::Unknown => self.toast(&format!(
                "Don't know how to open {} — supported: .iso, .mkv",
                path.display()
            )),
        }
    }

    fn scan_mkv(&self, path: PathBuf) {
        tracing::info!("identifying MKV {}", path.display());
        let display_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let scanning_page = self.push_scanning_page(&format!("Probing {display_name}…"));

        let (tx, rx) = async_channel::bounded(1);
        let path_for_task = path.clone();
        crate::runtime::tokio_runtime().spawn(async move {
            let _ = tx.send(identify_mkv(path_for_task).await).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = window)]
            self,
            async move {
                let result = rx.recv().await;
                window.dismiss_scanning_page(&scanning_page);
                match result {
                    Ok(Ok(result)) => window.show_identification(result),
                    Ok(Err(e)) => {
                        tracing::error!("MKV identify failed: {e:#}");
                        window.toast(&format!("Probe failed: {e}"));
                    }
                    Err(e) => {
                        tracing::error!("scan channel closed: {e}");
                        window.toast("Probe worker stopped unexpectedly");
                    }
                }
            }
        ));
    }

    fn scan_iso(&self, path: PathBuf) {
        tracing::info!("identifying {}", path.display());
        let display_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let scanning_page = self.push_scanning_page(&format!("Scanning {display_name}…"));

        let (tx, rx) = async_channel::bounded(1);
        let path_for_task = path.clone();
        crate::runtime::tokio_runtime().spawn(async move {
            let _ = tx.send(identify_iso(path_for_task).await).await;
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = window)]
            self,
            async move {
                let result = rx.recv().await;
                window.dismiss_scanning_page(&scanning_page);
                match result {
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

    /// Push a transient "scanning…" NavigationPage with a centered spinner
    /// and the supplied message. Returned page should be passed to
    /// `dismiss_scanning_page` when the work finishes so the page is removed
    /// from the navigation stack before any next page is pushed.
    fn push_scanning_page(&self, message: &str) -> adw::NavigationPage {
        let spinner = adw::Spinner::new();
        spinner.set_size_request(64, 64);

        let label = gtk::Label::builder()
            .label(message)
            .css_classes(["title-2"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .spacing(18)
            .build();
        content.append(&spinner);
        content.append(&label);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));

        let page = adw::NavigationPage::builder()
            .title("Scanning")
            .child(&toolbar)
            .can_pop(false)
            .build();
        self.imp().nav.push(&page);
        page
    }

    /// Pop the scanning page if it's still the current one. No-op when
    /// the page was already replaced/popped (e.g. the user closed the
    /// window mid-scan).
    fn dismiss_scanning_page(&self, page: &adw::NavigationPage) {
        let nav = self.imp().nav.get();
        if nav.visible_page().as_ref() == Some(page) {
            nav.pop();
        }
    }

    pub(crate) fn show_identification(&self, result: IdentificationResult) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Iso,
    Mkv,
    Unknown,
}

fn input_kind(path: &std::path::Path) -> InputKind {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("iso") => InputKind::Iso,
        Some(ext) if ext.eq_ignore_ascii_case("mkv") => InputKind::Mkv,
        _ => InputKind::Unknown,
    }
}

#[cfg(test)]
fn is_iso_like(path: &std::path::Path) -> bool {
    matches!(input_kind(path), InputKind::Iso)
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
