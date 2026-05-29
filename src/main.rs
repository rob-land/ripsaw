use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "threedrip=debug,info".into()),
        )
        .init();

    gio::resources_register_include!("threedrip.gresource")
        .expect("register threedrip resources");

    threedrip::application::run()
}
