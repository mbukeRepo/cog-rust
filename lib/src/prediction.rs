use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use map_macro::hash_map;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use url::Url;

use crate::{
	errors::ValidationErrorSet,
	runner::{Error as RunnerError, Runner},
	shutdown::Shutdown,
	Cog,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Status {
	#[serde(skip)]
	Idle,

	Failed,
	Starting,
	Canceled,
	Succeeded,
	Processing,
}

pub type Extension = axum::Extension<Arc<RwLock<Prediction>>>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
	#[error("Attempted to re-initialize a prediction")]
	AlreadyRunning,

	#[error("Prediction is not yet complete")]
	NotComplete,

	#[error("The requested prediction does not exist")]
	Unknown,

	#[error("Failed to wait for prediction: {0}")]
	ReceiverError(#[from] flume::RecvError),

	#[error("Failed to run prediction: {0}")]
	Validation(#[from] ValidationErrorSet),
}

pub struct Prediction {
	runner: Runner,
	status: Status,
	pub id: Option<String>,
	pub shutdown: Shutdown,
	request: Option<Request>,
	cancel: flume::Sender<()>,
	response: Option<Response>,
	complete: Option<flume::Receiver<Response>>,
}

impl Prediction {
	pub fn setup<T: Cog + 'static>(shutdown: Shutdown) -> Self {
		let (cancel_tx, cancel_rx) = flume::unbounded();

		Self {
			id: None,
			request: None,
			complete: None,
			response: None,
			status: Status::Idle,
			shutdown: shutdown.clone(),
			cancel: cancel_tx,
			runner: Runner::new::<T>(shutdown, cancel_rx),
		}
	}

	pub fn init(&mut self, id: Option<String>, req: Request) -> Result<&mut Self, Error> {
		if !matches!(self.status, Status::Idle) {
			return Err(Error::AlreadyRunning);
		}

		self.id = id;
		self.request = Some(req);
		self.status = Status::Starting;

		Ok(self)
	}

	pub async fn run(&mut self) -> Result<Response, Error> {
		self.process()?.await;

		self.result()
	}

	pub async fn wait_for(&self, id: String) -> Result<Response, Error> {
		if self.id != Some(id) {
			return Err(Error::Unknown);
		}

		if let Some(response) = self.response.clone() {
			return Ok(response);
		}

		if !matches!(self.status, Status::Processing) {
			return Err(Error::AlreadyRunning);
		}

		// If the previous receiver was dropped, the prediction is complete
		if self.complete.as_ref().unwrap().is_disconnected() {
			return Err(Error::Unknown);
		}

		let complete = self.complete.as_ref().unwrap();
		Ok(complete.recv_async().await?)
	}

	pub fn process(&mut self) -> Result<impl Future<Output = ()> + '_, Error> {
		if !matches!(self.status, Status::Starting) {
			return Err(Error::AlreadyRunning);
		}

		let req = self.request.clone().unwrap();
		self.runner
			.validate(&req.input)
			.map_err(|e| e.fill_loc(&["body", "input"]))?;

		self.status = Status::Processing;

		let (complete_tx, complete_rx) = flume::bounded(1);
		self.complete = Some(complete_rx);

		Ok(async move {
			tokio::select! {
				_ = self.shutdown.handle() => {
					return;
				},
				output = self.runner.run(req.input.clone()) => {
					match output {
						Ok((output, predict_time)) => {
							self.status = Status::Succeeded;
							self.response = Some(Response::success(self.id.clone(), req, output, predict_time));
						},
						Err(RunnerError::Canceled) => {
							self.status = Status::Canceled;
							self.response = Some(Response::canceled(self.id.clone(), req));
						},
						Err(error) => {
							self.status = Status::Failed;
							self.response = Some(Response::error(self.id.clone(), req, &error));
						}
					}
				}
			}
			complete_tx.send(self.response.clone().unwrap()).unwrap();
		})
	}

	pub fn result(&mut self) -> Result<Response, Error> {
		if !matches!(self.status, Status::Succeeded | Status::Failed) {
			return Err(Error::NotComplete);
		}

		let response = self.response.clone().ok_or(Error::NotComplete)?;
		self.reset();

		Ok(response)
	}

	pub fn cancel(&mut self, id: String) -> Result<&mut Self, Error> {
		if self.id != Some(id) {
			return Err(Error::Unknown);
		}

		if !matches!(self.status, Status::Processing) {
			return Err(Error::AlreadyRunning);
		}

		self.cancel.send(()).unwrap();
		self.status = Status::Canceled;

		Ok(self)
	}

	fn reset(&mut self) {
		self.id = None;
		self.request = None;
		self.response = None;
		self.complete = None;
		self.status = Status::Idle;
	}

	pub fn extension(self) -> Extension {
		axum::Extension(Arc::new(RwLock::new(self)))
	}
}

pub struct SyncGuard<'a> {
	prediction: tokio::sync::RwLockWriteGuard<'a, Prediction>,
}

impl<'a> SyncGuard<'a> {
	pub fn new(prediction: tokio::sync::RwLockWriteGuard<'a, Prediction>) -> Self {
		Self { prediction }
	}

	pub fn init(&mut self, id: Option<String>, req: Request) -> Result<&mut Self, Error> {
		self.prediction.init(id, req)?;
		Ok(self)
	}

	pub async fn run(&mut self) -> Result<Response, Error> {
		self.prediction.run().await
	}
}

impl Drop for SyncGuard<'_> {
	fn drop(&mut self) {
		self.prediction.cancel.send(()).unwrap();
		self.prediction.reset();
	}
}

#[derive(Debug, Clone, serde::Deserialize, JsonSchema)]
pub enum WebhookEvent {
	Start,
	Output,
	Logs,
	Completed,
}

#[derive(Debug, Clone, serde::Deserialize, JsonSchema)]
pub struct Request<T = Value> {
	pub webhook: Option<Url>,
	pub webhook_event_filters: Option<Vec<WebhookEvent>>,
	pub output_file_prefix: Option<String>,

	pub input: T,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Response<Req = Value, Res = Value> {
	pub input: Option<Req>,
	pub output: Option<Res>,

	pub id: Option<String>,
	pub version: Option<String>,

	pub created_at: Option<DateTime<Utc>>,
	pub started_at: Option<DateTime<Utc>>,
	pub completed_at: Option<DateTime<Utc>>,

	pub logs: String,
	pub status: Status,
	pub error: Option<String>,

	metrics: Option<HashMap<String, Value>>,
}

impl Response {
	pub fn success(
		id: Option<String>,
		req: Request,
		output: Value,
		predict_time: Duration,
	) -> Self {
		Self {
			id,
			output: Some(output),
			input: Some(req.input),
			status: Status::Succeeded,
			metrics: Some(hash_map! {
				"predict_time".to_string() => predict_time.as_secs_f64().into()
			}),
			..Self::default()
		}
	}
	pub fn error(id: Option<String>, req: Request, error: &RunnerError) -> Self {
		Self {
			id,
			input: Some(req.input),
			status: Status::Failed,
			error: Some(error.to_string()),
			..Self::default()
		}
	}
	pub fn canceled(id: Option<String>, req: Request) -> Self {
		Self {
			id,
			input: Some(req.input),
			status: Status::Canceled,
			..Self::default()
		}
	}
}

impl Default for Response {
	fn default() -> Self {
		Self {
			id: None,
			error: None,
			input: None,
			output: None,
			metrics: None,
			version: None,
			created_at: None,
			logs: String::new(),
			status: Status::Starting,
			started_at: Utc::now().into(),
			completed_at: Utc::now().into(),
		}
	}
}
