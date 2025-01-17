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
use tracing::{debug, error, trace};

use crate::{
	action,
	config::{InitConfig, RuntimeConfig},
	error::{CriticalError, ReconfigError, RuntimeError},
	event::Event,
	fs,
	handler::{rte, Handler},
	signal,
};

/// The main watchexec runtime.
///
/// All this really does is tie the pieces together in one convenient interface.
///
/// It creates the correct channels, spawns every available event sources, the action worker, the
/// error hook, and provides an interface to change the runtime configuration during the runtime,
/// inject synthetic events, and wait for graceful shutdown.
pub struct Watchexec {
	handle: Arc<AtomicTake<JoinHandle<Result<(), CriticalError>>>>,
	start_lock: Arc<Notify>,

	action_watch: watch::Sender<action::WorkingData>,
	fs_watch: watch::Sender<fs::WorkingData>,

	event_input: mpsc::Sender<Event>,
}

impl fmt::Debug for Watchexec {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Watchexec").finish_non_exhaustive()
	}
}

impl Watchexec {
	/// Instantiates a new `Watchexec` runtime from configuration.
	///
	/// Returns an [`Arc`] for convenience; use [`try_unwrap`][Arc::try_unwrap()] to get the value
	/// directly if needed.
	pub fn new(
		mut init: InitConfig,
		mut runtime: RuntimeConfig,
	) -> Result<Arc<Self>, CriticalError> {
		debug!(?init, ?runtime, pid=%std::process::id(), "initialising");

		let (ev_s, ev_r) = mpsc::channel(init.event_channel_size);
		let (ac_s, ac_r) = watch::channel(take(&mut runtime.action));
		let (fs_s, fs_r) = watch::channel(fs::WorkingData::default());

		let event_input = ev_s.clone();

		// TODO: figure out how to do this (aka start the fs work) after the main task start lock
		trace!("sending initial config to fs worker");
		fs_s.send(take(&mut runtime.fs))
			.expect("cannot send to just-created fs watch (bug)");

		trace!("creating main task");
		let notify = Arc::new(Notify::new());
		let start_lock = notify.clone();
		let handle = spawn(async move {
			trace!("waiting for start lock");
			notify.notified().await;
			debug!("starting main task");

			let (er_s, er_r) = mpsc::channel(init.error_channel_size);

			let eh = replace(&mut init.error_handler, Box::new(()) as _);

			macro_rules! subtask {
				($name:ident, $task:expr) => {{
					debug!(subtask=%stringify!($name), "spawning subtask");
					spawn($task).then(|jr| async { flatten(jr) })
				}};
			}

			let action = subtask!(
				action,
				action::worker(ac_r, er_s.clone(), ev_s.clone(), ev_r)
			);
			let fs = subtask!(fs, fs::worker(fs_r, er_s.clone(), ev_s.clone()));
			let signal = subtask!(signal, signal::source::worker(er_s.clone(), ev_s.clone()));

			let error_hook = subtask!(error_hook, error_hook(er_r, eh));

			try_join!(action, error_hook, fs, signal)
				.map(drop)
				.or_else(|e| {
					if matches!(e, CriticalError::Exit) {
						trace!("got graceful exit request via critical error, erasing the error");
						Ok(())
					} else {
						Err(e)
					}
				})
				.map(|_| {
					debug!("main task graceful exit");
				})
		});

		trace!("done with setup");
		Ok(Arc::new(Self {
			handle: Arc::new(AtomicTake::new(handle)),
			start_lock,

			action_watch: ac_s,
			fs_watch: fs_s,

			event_input,
		}))
	}

	/// Applies a new [`RuntimeConfig`] to the runtime.
	pub fn reconfigure(&self, config: RuntimeConfig) -> Result<(), ReconfigError> {
		debug!(?config, "reconfiguring");
		self.action_watch.send(config.action)?;
		self.fs_watch.send(config.fs)?;
		Ok(())
	}

	/// Inputs an [`Event`] directly.
	///
	/// This can be useful for testing, for custom event sources, or for one-off action triggers
	/// (for example, on start).
	///
	/// Hint: use [`Event::default()`] to send an empty event (which won't be filtered).
	pub async fn send_event(&self, event: Event) -> Result<(), CriticalError> {
		self.event_input.send(event).await?;
		Ok(())
	}

	/// Start watchexec and obtain the handle to its main task.
	///
	/// This must only be called once.
	///
	/// # Panics
	/// Panics if called twice.
	pub fn main(&self) -> JoinHandle<Result<(), CriticalError>> {
		trace!("notifying start lock");
		self.start_lock.notify_one();

		debug!("handing over main task handle");
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
		if matches!(err, RuntimeError::Exit) {
			trace!("got graceful exit request via runtime error, upgrading to crit");
			return Err(CriticalError::Exit);
		}

		error!(%err, "runtime error");
		if let Err(err) = handler.handle(err) {
			error!(%err, "error while handling error");
			handler
				.handle(rte("error hook", err))
				.unwrap_or_else(|err| {
					error!(%err, "error while handling error of handling error");
				});
		}
	}

	Ok(())
}
