use std::{
	fmt,
	mem::{replace, take},
	sync::Arc,
};

use atomic_take::AtomicTake;
use futures::FutureExt;
use tokio::{
	spawn,
	sync::{mpsc, watch, Notify},
	task::{JoinError, JoinHandle},
	try_join,
};

use crate::{
	action,
	config::{InitConfig, RuntimeConfig},
	error::{CriticalError, ReconfigError, RuntimeError},
	fs,
	handler::Handler,
	signal,
};

pub struct Watchexec {
	handle: Arc<AtomicTake<JoinHandle<Result<(), CriticalError>>>>,
	start_lock: Arc<Notify>,

	action_watch: watch::Sender<action::WorkingData>,
	fs_watch: watch::Sender<fs::WorkingData>,
}

impl fmt::Debug for Watchexec {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Watchexec").finish_non_exhaustive()
	}
}

impl Watchexec {
	/// TODO
	///
	/// Returns an [`Arc`] for convenience; use [`try_unwrap`][Arc::try_unwrap()] to get the value
	/// directly if needed.
	pub fn new(
		mut init: InitConfig,
		mut runtime: RuntimeConfig,
	) -> Result<Arc<Self>, CriticalError> {
		let (fs_s, fs_r) = watch::channel(take(&mut runtime.fs));
		let (ac_s, ac_r) = watch::channel(take(&mut runtime.action));

		let notify = Arc::new(Notify::new());
		let start_lock = notify.clone();
		let handle = spawn(async move {
			notify.notified().await;

			let (er_s, er_r) = mpsc::channel(init.error_channel_size);
			let (ev_s, ev_r) = mpsc::channel(init.event_channel_size);

			let eh = replace(&mut init.error_handler, Box::new(()) as _);

			macro_rules! subtask {
				($task:expr) => {
					spawn($task).then(|jr| async { flatten(jr) })
				};
			}

			let action = subtask!(action::worker(ac_r, er_s.clone(), ev_r));
			let fs = subtask!(fs::worker(fs_r, er_s.clone(), ev_s.clone()));
			let signal = subtask!(signal::worker(er_s.clone(), ev_s.clone()));

			let error_hook = subtask!(error_hook(er_r, eh));

			try_join!(action, error_hook, fs, signal).map(drop)
		});

		Ok(Arc::new(Self {
			handle: Arc::new(AtomicTake::new(handle)),
			start_lock,

			action_watch: ac_s,
			fs_watch: fs_s,
		}))
	}

	pub fn reconfig(&self, config: RuntimeConfig) -> Result<(), ReconfigError> {
		self.action_watch.send(config.action)?;
		self.fs_watch.send(config.fs)?;
		Ok(())
	}

	/// Start watchexec and obtain the handle to its main task.
	///
	/// This must only be called once.
	///
	/// # Panics
	/// Panics if called twice.
	pub fn main(&self) -> JoinHandle<Result<(), CriticalError>> {
		self.start_lock.notify_one();
		self.handle
			.take()
			.expect("Watchexec::main was called twice")
	}
}

#[inline]
fn flatten(join_res: Result<Result<(), CriticalError>, JoinError>) -> Result<(), CriticalError> {
	join_res
		.map_err(CriticalError::MainTaskJoin)
		.and_then(|x| x)
}

async fn error_hook(
	mut errors: mpsc::Receiver<RuntimeError>,
	mut handler: Box<dyn Handler<RuntimeError> + Send>,
) -> Result<(), CriticalError> {
	while let Some(err) = errors.recv().await {
		if let Err(e) = handler.handle(err) {
			handler
				.handle(RuntimeError::Handler {
					ctx: "error hook",
					err: e.to_string(),
				})
				.ok();
		}
	}

	Ok(())
}