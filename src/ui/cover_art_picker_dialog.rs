// Cover-art picker. Presents the highest-voted TMDB posters in a
// FlowBox; user clicks one + hits Confirm. Selection callback fires
// with the picked image's `file_path` so the caller can download the
// full-resolution version for the staged submission's cover.jpg.
//
// We deliberately don't download the full-size image here -- the
// dialog only fetches w185 thumbnails (~30 KB each) for display.
// The caller handles the original-size download because it owns the
// destination buffer + the loading-spinner UX around it.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::glib::{self, clone};
use gtk::{gio, CompositeTemplate};

use crate::identify::tmdb::TmdbImage;

/// w185 is the smallest TMDB poster size that still looks reasonable
/// at the thumbnail scale our FlowBox uses (~180-200 px wide).
const THUMB_SIZE: &str = "w185";

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/land/rob/ripsaw/ui/cover-art-picker-dialog.ui")]
    pub struct CoverArtPickerDialog {
        #[template_child] pub stack: TemplateChild<gtk::Stack>,
        #[template_child] pub flowbox: TemplateChild<gtk::FlowBox>,
        #[template_child] pub confirm_button: TemplateChild<gtk::Button>,
        #[allow(dead_code)]
        #[template_child] pub status_bin: TemplateChild<adw::Bin>,

        /// Posters bound to the rows in `flowbox`, in selection order.
        pub posters: RefCell<Vec<TmdbImage>>,
        /// The currently-selected file_path (matches one of the
        /// posters above). `None` until the user clicks a child.
        pub selected_file_path: RefCell<Option<String>>,
        /// Callback fired when the user clicks "Use selected" with a
        /// valid selection. Receives the picked `TmdbImage` so the
        /// caller has the file_path + width/height for the actual
        /// download. The dialog is closed before the callback fires.
        pub on_confirm: RefCell<Option<Box<dyn Fn(TmdbImage)>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CoverArtPickerDialog {
        const NAME: &'static str = "RipsawCoverArtPickerDialog";
        type Type = super::CoverArtPickerDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }
        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for CoverArtPickerDialog {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_actions();
            self.obj().wire_selection();
        }
    }
    impl WidgetImpl for CoverArtPickerDialog {}
    impl AdwDialogImpl for CoverArtPickerDialog {}
}

glib::wrapper! {
    pub struct CoverArtPickerDialog(ObjectSubclass<imp::CoverArtPickerDialog>)
        @extends gtk::Widget, adw::Dialog,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for CoverArtPickerDialog {
    fn default() -> Self { glib::Object::new() }
}

impl CoverArtPickerDialog {
    /// Drop the posters in (already-sorted by caller) into the
    /// FlowBox. Each thumbnail is downloaded asynchronously and
    /// painted into a `gtk::Picture` as bytes arrive. The dialog
    /// shows a spinner until the first download finishes.
    pub fn load_posters(&self, posters: Vec<TmdbImage>) {
        if posters.is_empty() {
            self.imp().stack.set_visible_child_name("empty");
            return;
        }
        self.imp().posters.replace(posters.clone());
        // Cap at 9 so the user isn't drowning in low-vote variants.
        for (idx, img) in posters.into_iter().take(9).enumerate() {
            let child = build_poster_child(&img, idx);
            self.imp().flowbox.append(&child);
            spawn_thumb_download(&child, &img);
        }
        self.imp().stack.set_visible_child_name("grid");
    }

    /// Register a callback for when the user picks a poster. Replaces
    /// any previously-set callback.
    pub fn on_confirm(&self, cb: impl Fn(TmdbImage) + 'static) {
        self.imp().on_confirm.replace(Some(Box::new(cb)));
    }

    fn setup_actions(&self) {
        let confirm = gio::SimpleAction::new("confirm", None);
        confirm.connect_activate(clone!(
            #[weak(rename_to = dialog)] self,
            move |_, _| dialog.confirm_selection()
        ));
        let group = gio::SimpleActionGroup::new();
        group.add_action(&confirm);
        self.insert_action_group("picker", Some(&group));
    }

    fn wire_selection(&self) {
        self.imp().flowbox.connect_selected_children_changed(clone!(
            #[weak(rename_to = dialog)] self,
            move |fb| {
                let picked = fb.selected_children().into_iter().next();
                let file_path = picked
                    .and_then(|c| c.child())
                    .and_then(|w| w.downcast::<gtk::Box>().ok())
                    .and_then(|b| {
                        // The child Box's first non-Picture meaningful
                        // attribute is the widget-name we stashed at
                        // build time -- it holds the file_path.
                        let name = b.widget_name().to_string();
                        if name.is_empty() { None } else { Some(name) }
                    });
                let has_choice = file_path.is_some();
                dialog.imp().selected_file_path.replace(file_path);
                dialog.imp().confirm_button.set_sensitive(has_choice);
            }
        ));
    }

    fn confirm_selection(&self) {
        let Some(file_path) = self.imp().selected_file_path.borrow().clone() else {
            return;
        };
        let posters = self.imp().posters.borrow().clone();
        let Some(picked) = posters.into_iter().find(|p| p.file_path == file_path) else {
            return;
        };
        let cb = self.imp().on_confirm.replace(None);
        self.close();
        if let Some(cb) = cb {
            cb(picked);
        }
    }
}

/// Construct one FlowBox child. The widget tree is:
///
///   FlowBoxChild
///   └── Box (vertical) — widget_name = poster.file_path
///       ├── Picture (set later by spawn_thumb_download)
///       └── Label (size + vote)
///
/// We stash the file_path on the inner Box's widget_name so the
/// selection callback can recover the choice without keeping a side
/// map. `_idx` is the position in the FlowBox if a caller wants to
/// re-discover the order later.
fn build_poster_child(img: &TmdbImage, _idx: usize) -> gtk::FlowBoxChild {
    let pic = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .width_request(180)
        .height_request(270)
        .build();
    pic.set_widget_name("thumb");

    let label_text = format!(
        "{}×{}  ·  vote {:.1}",
        img.width, img.height, img.vote_average
    );
    let label = gtk::Label::builder()
        .label(&label_text)
        .css_classes(["caption", "dim-label"])
        .build();

    let bx = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    bx.set_widget_name(&img.file_path);
    bx.append(&pic);
    bx.append(&label);

    gtk::FlowBoxChild::builder().child(&bx).build()
}

/// Fetch a w185 thumbnail and paint it into the FlowBoxChild's
/// Picture widget. Best-effort: failures are logged and the
/// Picture stays empty (a translucent placeholder) but the row is
/// still selectable.
fn spawn_thumb_download(child: &gtk::FlowBoxChild, img: &TmdbImage) {
    let file_path = img.file_path.clone();
    let child_weak = Rc::new(child.downgrade());
    let (tx, rx) = async_channel::bounded::<Option<Vec<u8>>>(1);
    crate::runtime::tokio_runtime().spawn(async move {
        let bytes = download_thumb(&file_path).await.ok();
        let _ = tx.send(bytes).await;
    });
    glib::MainContext::default().spawn_local(clone!(
        #[strong] child_weak,
        async move {
            let Ok(bytes_opt) = rx.recv().await else { return; };
            let Some(bytes) = bytes_opt else { return; };
            let Some(child) = child_weak.upgrade() else { return; };
            let Some(bx) = child.child().and_then(|w| w.downcast::<gtk::Box>().ok())
            else { return; };
            let Some(picture) = bx
                .first_child()
                .and_then(|w| w.downcast::<gtk::Picture>().ok())
            else { return; };
            match gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) {
                Ok(texture) => picture.set_paintable(Some(&texture)),
                Err(e) => tracing::warn!("decode TMDB thumb failed: {e}"),
            }
        }
    ));
}

async fn download_thumb(file_path: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!("https://image.tmdb.org/t/p/{THUMB_SIZE}{file_path}");
    let bytes = reqwest::get(&url).await?.error_for_status()?.bytes().await?;
    Ok(bytes.to_vec())
}
