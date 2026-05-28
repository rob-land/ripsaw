// Optical drive detection via udisks2. See docs/rip.md § "Drive detection".

#[derive(Debug, Clone)]
pub struct Drive {
    pub device: std::path::PathBuf, // e.g. /dev/sr0
    pub vendor: String,
    pub model: String,
    pub disc_present: bool,
    pub disc_label: Option<String>,
    pub disc_kind: Option<DiscKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscKind {
    Dvd,
    BluRay,
    UltraHdBluRay,
}

pub async fn enumerate() -> anyhow::Result<Vec<Drive>> {
    todo!("zbus call to org.freedesktop.UDisks2, filter for OpticalDrive interface")
}

pub fn watch(_callback: impl Fn(DriveEvent) + Send + 'static) {
    todo!("subscribe to udisks2 InterfacesAdded/Removed and PropertiesChanged")
}

#[derive(Debug, Clone)]
pub enum DriveEvent {
    Inserted(Drive),
    Ejected(Drive),
    Updated(Drive),
}
