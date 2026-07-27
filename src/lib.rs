pub mod application;
pub mod convert;
pub mod integrations;
pub mod runtime;
pub mod state;
pub mod ui;
pub mod window;
pub mod settings;

pub mod identify;
pub mod rip;
pub mod transcode;
pub mod naming;

pub const APP_ID: &str = "land.rob.ripsaw";
pub const APP_NAME: &str = "Ripsaw";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
