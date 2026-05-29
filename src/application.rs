use adw::prelude::*;
use anyhow::Result;
use gtk::gio;
use gtk::glib::clone;

pub fn run() -> Result<()> {
    let app = adw::Application::builder()
        .application_id(crate::APP_ID)
        .resource_base_path("/land/rob/ripsaw/")
        .build();

    app.connect_startup(|app| {
        register_actions(app);
    });

    app.connect_activate(|app| {
        let window = crate::window::RipsawWindow::new(app);
        window.present();
    });

    app.connect_shutdown(|_| {
        crate::runtime::tokio_runtime().block_on(crate::state::cleanup_mounts());
    });

    app.run();
    Ok(())
}

fn register_actions(app: &adw::Application) {
    let about = gio::ActionEntry::builder("about")
        .activate(clone!(
            #[weak]
            app,
            move |_app: &adw::Application, _action, _param| {
                show_about(&app);
            }
        ))
        .build();

    let preferences = gio::ActionEntry::builder("preferences")
        .activate(clone!(
            #[weak]
            app,
            move |_app: &adw::Application, _action, _param| {
                show_preferences(&app);
            }
        ))
        .build();

    let quit = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _action, _param| {
            app.quit();
        })
        .build();

    app.add_action_entries([about, preferences, quit]);
    app.set_accels_for_action("app.quit", &["<Primary>q"]);
    app.set_accels_for_action("app.preferences", &["<Primary>comma"]);
    app.set_accels_for_action("win.open-iso", &["<Primary>o"]);
    app.set_accels_for_action("win.open-disc", &["<Primary>d"]);
    app.set_accels_for_action("win.show-help-overlay", &["<Primary>question"]);
}

fn show_preferences(app: &adw::Application) {
    let dialog = crate::ui::preferences_dialog::PreferencesDialog::default();
    if let Some(window) = app.active_window() {
        dialog.present(Some(&window));
    } else {
        dialog.present(None::<&gtk::Window>);
    }
}

fn show_about(app: &adw::Application) {
    let dialog = adw::AboutDialog::builder()
        .application_name("Ripsaw")
        .application_icon(crate::APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("The Ripsaw Contributors")
        .website("https://git.rob.land/rob/ripsaw")
        .copyright("© 2026 The Ripsaw Authors")
        .license_type(gtk::License::Gpl30)
        .comments("GTK4/libadwaita disc-ripping frontend for MakeMKV.\n\nDisc identification via TheDiscDB, Jellyfin/Plex/Kodi naming, and a future 3D MVC pipeline.")
        .build();

    if let Some(window) = app.active_window() {
        dialog.present(Some(&window));
    } else {
        dialog.present(None::<&gtk::Window>);
    }
}
