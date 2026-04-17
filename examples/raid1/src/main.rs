use ironspdk::SpdkApp;
use log::{error, info};

mod raid1;

fn main() {
    env_logger::init();

    let mut app = SpdkApp::new("raid1");
    app.on_start(|| {
        info!("Rust SPDK RAID1 application is started");
    });
    app.on_shutdown(|| {
        info!("Rust SPDK RAID1 application is shutting down");
    });
    if let Err(err) = app.run() {
        error!("SPDK error: {}", err);
    }
}
