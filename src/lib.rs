pub mod application;
pub mod convert;
pub mod runtime;
pub mod state;
pub mod ui;
pub mod window;
pub mod settings;

pub mod identify;
pub mod rip;
pub mod transcode;
pub mod mvc;
pub mod naming;

pub const APP_ID: &str = "dev.threedrip.ThreeDrip";
pub const APP_NAME: &str = "3drip";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
