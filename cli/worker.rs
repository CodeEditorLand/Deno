// Copyright 2018-2019 the Deno authors. All rights reserved. MIT license.
use std::{
	env,
	future::Future,
	pin::Pin,
	sync::{Arc, Mutex},
	task::{Context, Poll},
};

use deno::{self, Buf, ErrBox, ModuleSpecifier, RecursiveLoad, StartupData};
use futures::{
	channel::mpsc,
	future::{FutureExt, TryFutureExt},
	sink::SinkExt,
	stream::StreamExt,
};
use url::Url;

use crate::{fmt_errors::JSError, ops, state::ThreadSafeState};

/// Wraps mpsc channels so they can be referenced
/// from ops and used to facilitate parent-child communication
/// for workers.
pub struct WorkerChannels {
	pub sender:mpsc::Sender<Buf>,
	pub receiver:mpsc::Receiver<Buf>,
}

/// Wraps deno::Isolate to provide source maps, ops for the CLI, and
/// high-level module loading.
#[derive(Clone)]
pub struct Worker {
	pub name:String,
	isolate:Arc<Mutex<deno::Isolate>>,
	pub state:ThreadSafeState,
	external_channels:Arc<Mutex<WorkerChannels>>,
}

impl Worker {
	pub fn new(
		name:String,
		startup_data:StartupData,
		state:ThreadSafeState,
		external_channels:WorkerChannels,
	) -> Self {
		let isolate = Arc::new(Mutex::new(deno::Isolate::new(startup_data, false)));
		{
			let mut i = isolate.lock().unwrap();
			let op_registry = i.op_registry.clone();

			ops::compiler::init(&mut i, &state);
			ops::errors::init(&mut i, &state);
			ops::fetch::init(&mut i, &state);
			ops::files::init(&mut i, &state);
			ops::fs::init(&mut i, &state);
			ops::io::init(&mut i, &state);
			ops::plugins::init(&mut i, &state, op_registry);
			ops::net::init(&mut i, &state);
			ops::tls::init(&mut i, &state);
			ops::os::init(&mut i, &state);
			ops::permissions::init(&mut i, &state);
			ops::process::init(&mut i, &state);
			ops::random::init(&mut i, &state);
			ops::repl::init(&mut i, &state);
			ops::resources::init(&mut i, &state);
			ops::timers::init(&mut i, &state);
			ops::workers::init(&mut i, &state);

			let state_ = state.clone();
			i.set_dyn_import(move |id, specifier, referrer| {
				let load_stream = RecursiveLoad::dynamic_import(
					id,
					specifier,
					referrer,
					state_.clone(),
					state_.modules.clone(),
				);
				Box::new(load_stream)
			});

			let global_state_ = state.global_state.clone();
			i.set_js_error_create(move |v8_exception| {
				JSError::from_v8_exception(v8_exception, &global_state_.ts_compiler)
			})
		}

		Self { name, isolate, state, external_channels:Arc::new(Mutex::new(external_channels)) }
	}

	/// Same as execute2() but the filename defaults to "$CWD/__anonymous__".
	pub fn execute(&mut self, js_source:&str) -> Result<(), ErrBox> {
		let path = env::current_dir().unwrap().join("__anonymous__");
		let url = Url::from_file_path(path).unwrap();
		self.execute2(url.as_str(), js_source)
	}

	/// Executes the provided JavaScript source code. The js_filename argument
	/// is provided only for debugging purposes.
	pub fn execute2(&mut self, js_filename:&str, js_source:&str) -> Result<(), ErrBox> {
		let mut isolate = self.isolate.lock().unwrap();
		isolate.execute(js_filename, js_source)
	}

	/// Executes the provided JavaScript module.
	pub fn execute_mod_async(
		&mut self,
		module_specifier:&ModuleSpecifier,
		maybe_code:Option<String>,
		is_prefetch:bool,
	) -> impl Future<Output = Result<(), ErrBox>> {
		let worker = self.clone();
		let loader = self.state.clone();
		let isolate = self.isolate.clone();
		let modules = self.state.modules.clone();
		let recursive_load =
			RecursiveLoad::main(&module_specifier.to_string(), maybe_code, loader, modules);

		async move {
			let id = recursive_load.get_future(isolate).await?;
			worker.state.global_state.progress.done();

			if !is_prefetch {
				let mut isolate = worker.isolate.lock().unwrap();
				return isolate.mod_evaluate(id);
			}

			Ok(())
		}
	}

	/// Post message to worker as a host.
	///
	/// This method blocks current thread.
	pub fn post_message(self: &Self, buf:Buf) -> impl Future<Output = Result<(), ErrBox>> {
		let channels = self.external_channels.lock().unwrap();
		let mut sender = channels.sender.clone();
		async move {
			let result = sender.send(buf).map_err(ErrBox::from).await;
			drop(sender);
			result
		}
	}

	/// Get message from worker as a host.
	pub fn get_message(self: &Self) -> WorkerReceiver {
		WorkerReceiver { channels:self.external_channels.clone() }
	}
}

impl Future for Worker {
	type Output = Result<(), ErrBox>;

	fn poll(self: Pin<&mut Self>, cx:&mut Context) -> Poll<Self::Output> {
		let inner = self.get_mut();
		let mut isolate = inner.isolate.lock().unwrap();
		isolate.poll_unpin(cx)
	}
}

/// This structure wraps worker's resource id to implement future
/// that will return message received from worker or None
/// if worker's channel has been closed.
pub struct WorkerReceiver {
	channels:Arc<Mutex<WorkerChannels>>,
}

impl Future for WorkerReceiver {
	type Output = Result<Option<Buf>, ErrBox>;

	fn poll(self: Pin<&mut Self>, cx:&mut Context) -> Poll<Self::Output> {
		let mut channels = self.channels.lock().unwrap();
		match channels.receiver.poll_next_unpin(cx) {
			Poll::Ready(v) => Poll::Ready(Ok(v)),
			Poll::Pending => Poll::Pending,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::Ordering;

	use futures::executor::block_on;

	use super::*;
	use crate::{
		flags,
		global_state::ThreadSafeGlobalState,
		progress::Progress,
		startup_data,
		state::ThreadSafeState,
		tokio_util,
	};

	pub fn run_in_task<F>(f:F)
	where
		F: FnOnce() + Send + 'static, {
		let fut = futures::future::lazy(move |_cx| {
			f();
			Ok(())
		});

		tokio_util::run(fut)
	}

	pub fn panic_on_error<I, E, F>(f:F) -> impl Future<Output = Result<I, ()>>
	where
		F: Future<Output = Result<I, E>>,
		E: std::fmt::Debug, {
		f.map_err(|err| panic!("Future got unexpected error: {:?}", err))
	}

	#[test]
	fn execute_mod_esm_imports_a() {
		let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("tests/esm_imports_a.js")
			.to_owned();
		let module_specifier = ModuleSpecifier::resolve_url_or_path(&p.to_string_lossy()).unwrap();
		let global_state = ThreadSafeGlobalState::new(
			flags::DenoFlags {
				argv:vec![String::from("./deno"), module_specifier.to_string()],
				..flags::DenoFlags::default()
			},
			Progress::new(),
		)
		.unwrap();
		let (int, ext) = ThreadSafeState::create_channels();
		let state =
			ThreadSafeState::new(global_state, None, Some(module_specifier.clone()), true, int)
				.unwrap();
		let state_ = state.clone();
		tokio_util::run(async move {
			let mut worker = Worker::new("TEST".to_string(), StartupData::None, state, ext);
			let result = worker.execute_mod_async(&module_specifier, None, false).await;
			if let Err(err) = result {
				eprintln!("execute_mod err {:?}", err);
			}
			panic_on_error(worker).await
		});

		let metrics = &state_.metrics;
		assert_eq!(metrics.resolve_count.load(Ordering::SeqCst), 2);
		// Check that we didn't start the compiler.
		assert_eq!(metrics.compiler_starts.load(Ordering::SeqCst), 0);
	}

	#[test]
	fn execute_mod_circular() {
		let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("tests/circular1.ts")
			.to_owned();
		let module_specifier = ModuleSpecifier::resolve_url_or_path(&p.to_string_lossy()).unwrap();
		let global_state = ThreadSafeGlobalState::new(
			flags::DenoFlags {
				argv:vec![String::from("deno"), module_specifier.to_string()],
				..flags::DenoFlags::default()
			},
			Progress::new(),
		)
		.unwrap();
		let (int, ext) = ThreadSafeState::create_channels();
		let state =
			ThreadSafeState::new(global_state, None, Some(module_specifier.clone()), true, int)
				.unwrap();
		let state_ = state.clone();
		tokio_util::run(async move {
			let mut worker = Worker::new("TEST".to_string(), StartupData::None, state, ext);
			let result = worker.execute_mod_async(&module_specifier, None, false).await;
			if let Err(err) = result {
				eprintln!("execute_mod err {:?}", err);
			}
			panic_on_error(worker).await
		});

		let metrics = &state_.metrics;
		// TODO  assert_eq!(metrics.resolve_count.load(Ordering::SeqCst), 2);
		// Check that we didn't start the compiler.
		assert_eq!(metrics.compiler_starts.load(Ordering::SeqCst), 0);
	}

	#[test]
	fn execute_006_url_imports() {
		let http_server_guard = crate::test_util::http_server();

		let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("cli/tests/006_url_imports.ts")
			.to_owned();
		let module_specifier = ModuleSpecifier::resolve_url_or_path(&p.to_string_lossy()).unwrap();
		let mut flags = flags::DenoFlags::default();
		flags.argv = vec![String::from("deno"), module_specifier.to_string()];
		flags.reload = true;
		let global_state = ThreadSafeGlobalState::new(flags, Progress::new()).unwrap();
		let (int, ext) = ThreadSafeState::create_channels();
		let state = ThreadSafeState::new(
			global_state.clone(),
			None,
			Some(module_specifier.clone()),
			true,
			int,
		)
		.unwrap();
		let global_state_ = global_state.clone();
		let state_ = state.clone();
		tokio_util::run(async move {
			let mut worker =
				Worker::new("TEST".to_string(), startup_data::deno_isolate_init(), state, ext);
			worker.execute("denoMain()").unwrap();
			let result = worker.execute_mod_async(&module_specifier, None, false).await;

			if let Err(err) = result {
				eprintln!("execute_mod err {:?}", err);
			}
			panic_on_error(worker).await
		});

		assert_eq!(state_.metrics.resolve_count.load(Ordering::SeqCst), 3);
		// Check that we've only invoked the compiler once.
		assert_eq!(global_state_.metrics.compiler_starts.load(Ordering::SeqCst), 1);
		drop(http_server_guard);
	}

	fn create_test_worker() -> Worker {
		let (int, ext) = ThreadSafeState::create_channels();
		let state =
			ThreadSafeState::mock(vec![String::from("./deno"), String::from("hello.js")], int);
		let mut worker =
			Worker::new("TEST".to_string(), startup_data::deno_isolate_init(), state, ext);
		worker.execute("denoMain()").unwrap();
		worker.execute("workerMain()").unwrap();
		worker
	}

	#[test]
	fn test_worker_messages() {
		run_in_task(|| {
			let mut worker = create_test_worker();
			let source = r#"
        onmessage = function(e) {
          console.log("msg from main script", e.data);
          if (e.data == "exit") {
            delete window.onmessage;
            return;
          } else {
            console.assert(e.data === "hi");
          }
          postMessage([1, 2, 3]);
          console.log("after postMessage");
        }
        "#;
			worker.execute(source).unwrap();

			let worker_ = worker.clone();

			let fut = async move {
				let r = worker.await;
				r.unwrap();
				Ok(())
			};

			tokio::spawn(fut.boxed().compat());

			let msg = json!("hi").to_string().into_boxed_str().into_boxed_bytes();

			let r = block_on(worker_.post_message(msg));
			assert!(r.is_ok());

			let maybe_msg = block_on(worker_.get_message()).unwrap();
			assert!(maybe_msg.is_some());
			// Check if message received is [1, 2, 3] in json
			assert_eq!(*maybe_msg.unwrap(), *b"[1,2,3]");

			let msg = json!("exit").to_string().into_boxed_str().into_boxed_bytes();
			let r = block_on(worker_.post_message(msg));
			assert!(r.is_ok());
		})
	}

	#[test]
	fn removed_from_resource_table_on_close() {
		run_in_task(|| {
			let mut worker = create_test_worker();
			worker.execute("onmessage = () => { delete window.onmessage; }").unwrap();

			let worker_ = worker.clone();
			let worker_future = worker
				.then(move |r| {
					println!("workers.rs after resource close");
					r.unwrap();
					futures::future::ok(())
				})
				.shared();

			let worker_future_ = worker_future.clone();
			tokio::spawn(worker_future_.then(|_:Result<(), ()>| futures::future::ok(())).compat());

			let msg = json!("hi").to_string().into_boxed_str().into_boxed_bytes();
			let r = block_on(worker_.post_message(msg));
			assert!(r.is_ok());

			block_on(worker_future).unwrap();
		})
	}

	#[test]
	fn execute_mod_resolve_error() {
		run_in_task(|| {
			// "foo" is not a valid module specifier so this should return an
			// error.
			let mut worker = create_test_worker();
			let module_specifier = ModuleSpecifier::resolve_url_or_path("does-not-exist").unwrap();
			let result = block_on(worker.execute_mod_async(&module_specifier, None, false));
			assert!(result.is_err());
		})
	}

	#[test]
	fn execute_mod_002_hello() {
		run_in_task(|| {
			// This assumes cwd is project root (an assumption made throughout
			// the tests).
			let mut worker = create_test_worker();
			let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
				.parent()
				.unwrap()
				.join("tests/002_hello.ts")
				.to_owned();
			let module_specifier =
				ModuleSpecifier::resolve_url_or_path(&p.to_string_lossy()).unwrap();
			let result = block_on(worker.execute_mod_async(&module_specifier, None, false));
			assert!(result.is_ok());
		})
	}
}
