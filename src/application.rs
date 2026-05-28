use adw::prelude::*;
use anyhow::Result;

pub fn run() -> Result<()> {
    let app = adw::Application::builder()
        .application_id(crate::APP_ID)
        .resource_base_path("/dev/threedrip/ThreeDrip/")
        .build();

    app.connect_activate(|app| {
        let window = crate::window::ThreeDripWindow::new(app);
        window.present();
    });

    app.run();
    Ok(())
}
