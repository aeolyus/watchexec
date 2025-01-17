use std::time::Duration;

use miette::Result;
use tokio::time::sleep;
use watchexec::{
	config::{InitConfig, RuntimeConfig},
	Watchexec,
};

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt::init();

	let mut init = InitConfig::default();
	init.on_error(|err| async move {
		eprintln!("Watchexec Runtime Error: {}", err);
		Ok::<(), std::convert::Infallible>(())
	});

	let runtime = RuntimeConfig::default();

	let wx = Watchexec::new(init, runtime)?;
	wx.main();

	// TODO: induce an error here

	sleep(Duration::from_secs(1)).await;

	Ok(())
}
