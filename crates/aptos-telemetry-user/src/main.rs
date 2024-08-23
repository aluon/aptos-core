use aptos_logger::{info, LoggerFilterUpdater};
use futures::channel::mpsc;
use std::time::Duration;

fn main() {
    let mut logger_builder = aptos_logger::Logger::builder();
    let logger = logger_builder.build();
    let logger_filter_updater = LoggerFilterUpdater::new(logger, logger_builder);
    let (_tx, rx) = mpsc::channel(128);

    if let Some(rt) = aptos_telemetry::service::start_anonymous_telemetry_service(
        Some(rx),
        Some(logger_filter_updater),
    ) {
        info!("Hello world!");

        rt.block_on(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
    }
}
