use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ThreeDripWindow;

    #[glib::object_subclass]
    impl ObjectSubclass for ThreeDripWindow {
        const NAME: &'static str = "ThreeDripWindow";
        type Type = super::ThreeDripWindow;
        type ParentType = adw::ApplicationWindow;
    }

    impl ObjectImpl for ThreeDripWindow {}
    impl WidgetImpl for ThreeDripWindow {}
    impl WindowImpl for ThreeDripWindow {}
    impl ApplicationWindowImpl for ThreeDripWindow {}
    impl AdwApplicationWindowImpl for ThreeDripWindow {}
}

glib::wrapper! {
    pub struct ThreeDripWindow(ObjectSubclass<imp::ThreeDripWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
                    gtk::Root, gtk::ShortcutManager;
}

impl ThreeDripWindow {
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }
}
