use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::CompositeTemplate;

use std::path::PathBuf;

use crate::rip::makemkv::{ExtractEvent, ExtractProgress};
use crate::rip::plan::PlannedTitle;

#[derive(Debug, Clone)]
pub struct RipQueueItem {
    pub title_index: u32,
    pub display_label: String,
    pub output_dir: PathBuf,
    pub expected_output_filename: String,
    pub final_path: Option<PathBuf>,
    pub chapter_titles: Vec<String>,
    pub segment_title: Option<String>,
    pub conversion_format: Option<crate::convert::format::OutputFormat>,
}

impl From<PlannedTitle> for RipQueueItem {
    fn from(p: PlannedTitle) -> Self {
        RipQueueItem {
            title_index: p.title_index,
            display_label: p.display_label,
            output_dir: p.output_dir,
            expected_output_filename: p.output_filename,
            final_path: p.final_path,
            chapter_titles: p.chapter_titles,
            segment_title: p.segment_title,
            conversion_format: p.conversion_format,
        }
    }
}

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/land/rob/ripsaw/ui/rip-progress-page.ui")]
    pub struct RipProgressPage {
        #[template_child] pub current_label: TemplateChild<adw::ActionRow>,
        #[template_child] pub current_progress: TemplateChild<gtk::ProgressBar>,
        #[template_child] pub total_progress: TemplateChild<gtk::ProgressBar>,
        #[template_child] pub queue_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child] pub log_view: TemplateChild<gtk::TextView>,

        pub queue_rows: RefCell<Vec<adw::ActionRow>>,
        pub success_count: Cell<usize>,
        pub failure_count: Cell<usize>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RipProgressPage {
        const NAME: &'static str = "RipsawRipProgressPage";
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
        // Reset counters for the new queue (the same page may be reused
        // if the user starts a second rip without navigating away).
        self.imp().success_count.set(0);
        self.imp().failure_count.set(0);
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
        // Update aggregate success/failure counters so finish_queue can
        // render an accurate summary instead of "all titles complete".
        match &output {
            Ok(_) => {
                let n = self.imp().success_count.get() + 1;
                self.imp().success_count.set(n);
            }
            Err(_) => {
                let n = self.imp().failure_count.get() + 1;
                self.imp().failure_count.set(n);
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
        let ok = self.imp().success_count.get();
        let failed = self.imp().failure_count.get();
        let total = ok + failed;
        let (title, subtitle) = match (ok, failed) {
            (_, 0) => ("All titles done".to_string(), format!("{ok} succeeded")),
            (0, _) => (
                "Rip failed".to_string(),
                format!("{failed} of {total} titles failed — see queue rows for details"),
            ),
            _ => (
                format!("Finished with {failed} failure(s)"),
                format!("{ok} succeeded, {failed} failed of {total}"),
            ),
        };
        self.imp().current_progress.set_fraction(1.0);
        self.imp()
            .current_progress
            .set_text(Some(if failed == 0 { "complete" } else { "complete with failures" }));
        self.imp().total_progress.set_fraction(1.0);
        self.imp().current_label.set_title(&title);
        self.imp().current_label.set_subtitle(&subtitle);
    }
}

