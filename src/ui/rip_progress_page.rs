use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::CompositeTemplate;

use crate::identify::pipeline::IdentificationResult;
use crate::rip::makemkv::{ExtractEvent, ExtractProgress};
use crate::rip::makemkv_parse::TitleAttributes;

#[derive(Debug, Clone)]
pub struct RipQueueItem {
    pub title_index: u32,
    pub display_label: String,
    pub expected_output_filename: String,
}

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/dev/threedrip/ThreeDrip/ui/rip-progress-page.ui")]
    pub struct RipProgressPage {
        #[template_child] pub current_label: TemplateChild<adw::ActionRow>,
        #[template_child] pub current_progress: TemplateChild<gtk::ProgressBar>,
        #[template_child] pub total_progress: TemplateChild<gtk::ProgressBar>,
        #[template_child] pub queue_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub log_view: TemplateChild<gtk::TextView>,

        pub queue_rows: RefCell<Vec<adw::ActionRow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RipProgressPage {
        const NAME: &'static str = "ThreeDripRipProgressPage";
        type Type = super::RipProgressPage;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) { klass.bind_template(); }
        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) { obj.init_template(); }
    }

    impl ObjectImpl for RipProgressPage {}
    impl WidgetImpl for RipProgressPage {}
    impl NavigationPageImpl for RipProgressPage {}
}

glib::wrapper! {
    pub struct RipProgressPage(ObjectSubclass<imp::RipProgressPage>)
        @extends gtk::Widget, adw::NavigationPage,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for RipProgressPage {
    fn default() -> Self { glib::Object::new() }
}

impl RipProgressPage {
    pub fn set_queue(&self, items: &[RipQueueItem]) {
        let group = self.imp().queue_group.get();
        let mut rows = self.imp().queue_rows.borrow_mut();
        for row in rows.drain(..) {
            group.remove(&row);
        }
        for item in items {
            let row = adw::ActionRow::builder()
                .title(&item.display_label)
                .subtitle("queued")
                .build();
            group.add(&row);
            rows.push(row);
        }
    }

    pub fn mark_started(&self, index_in_queue: usize, item: &RipQueueItem) {
        self.imp().current_label.set_title(&item.display_label);
        self.imp().current_label.set_subtitle(&format!(
            "Title {} → {}",
            item.title_index, item.expected_output_filename,
        ));
        self.imp().current_progress.set_fraction(0.0);
        self.imp().current_progress.set_text(Some("starting…"));
        let rows = self.imp().queue_rows.borrow();
        if let Some(row) = rows.get(index_in_queue) {
            row.set_subtitle("running");
        }
    }

    pub fn mark_finished(&self, index_in_queue: usize, output: anyhow::Result<std::path::PathBuf>) {
        let rows = self.imp().queue_rows.borrow();
        if let Some(row) = rows.get(index_in_queue) {
            match &output {
                Ok(p) => row.set_subtitle(&format!("done • {}", p.display())),
                Err(e) => row.set_subtitle(&format!("failed • {e}")),
            }
        }
        let queue_count = rows.len() as f64;
        if queue_count > 0.0 {
            self.imp().total_progress.set_fraction(
                (index_in_queue as f64 + 1.0) / queue_count,
            );
            self.imp().total_progress.set_text(Some(&format!(
                "{} / {}", index_in_queue + 1, queue_count as usize,
            )));
        }
    }

    pub fn apply_event(&self, event: &ExtractEvent) {
        match event {
            ExtractEvent::Progress(p) => self.apply_progress(p),
            ExtractEvent::Message(msg) => self.append_log(&format!(
                "[{}] {}", msg.code, msg.text
            )),
        }
    }

    fn apply_progress(&self, p: &ExtractProgress) {
        self.imp().current_progress.set_fraction(p.current_fraction() as f64);
        self.imp().current_progress.set_text(Some(&format!(
            "{} — {:.0}%",
            p.current_label.as_deref().unwrap_or(""),
            p.current_fraction() * 100.0,
        )));
        // Overall progress within current title is total/max; we leave
        // the queue-wise total bar for mark_finished.
        let title_caption = p.total_label.as_deref().unwrap_or("");
        if !title_caption.is_empty() {
            self.imp().current_label.set_subtitle(title_caption);
        }
    }

    pub fn append_log(&self, line: &str) {
        let buf = self.imp().log_view.buffer();
        let mut end = buf.end_iter();
        buf.insert(&mut end, line);
        buf.insert(&mut end, "\n");
    }

    pub fn finish_queue(&self) {
        self.imp().current_progress.set_fraction(1.0);
        self.imp().current_progress.set_text(Some("complete"));
        self.imp().total_progress.set_fraction(1.0);
        self.imp().current_label.set_title("All titles done");
        self.imp().current_label.set_subtitle("");
    }
}

pub fn queue_from_selection(
    identification: &IdentificationResult,
    selected_indexes: &[u32],
) -> Vec<RipQueueItem> {
    selected_indexes
        .iter()
        .filter_map(|idx| {
            identification
                .scan
                .titles
                .iter()
                .find(|t| t.index == *idx)
                .map(|t| RipQueueItem {
                    title_index: t.index,
                    display_label: rip_label(t),
                    expected_output_filename: t.output_file.clone().unwrap_or_else(|| {
                        format!("title_t{:02}.mkv", t.index)
                    }),
                })
        })
        .collect()
}

fn rip_label(t: &TitleAttributes) -> String {
    let name = t.name.as_deref().filter(|s| !s.is_empty()).unwrap_or("(untitled)");
    format!("Title {} — {}", t.index, name)
}
