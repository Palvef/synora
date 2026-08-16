//! REST DTOs + typed client (spec §46/§9). The worker talks to the manager
//! exclusively through this crate; the wire format is the contract.

use serde::{Deserialize, Serialize};
use synora_core::job::JobSpec;

pub const API_V1: &str = "/api/v1";

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDTO {
    pub name: String,
    pub enabled: bool,
    pub status: String,
    pub worker: Option<String>,
    pub provider: String,
    pub upstream: Option<String>,
    pub storage_path: String,
    pub schedule: String,
    pub next_run: Option<i64>,
    pub last_run: Option<RunDTO>,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDTO {
    pub id: String,
    pub job_id: String,
    pub worker_id: Option<String>,
    pub status: String,
    pub retry_count: u32,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub duration_secs: Option<i64>,
    pub exit_code: Option<i64>,
    pub size_before: Option<i64>,
    pub size_after: Option<i64>,
    pub bytes_transferred: Option<i64>,
    pub message: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDTO {
    pub id: String,
    pub hostname: String,
    pub address: String,
    pub version: String,
    pub labels: Vec<String>,
    pub capabilities: serde_json::Value,
    pub status: String,
    pub jobs_running: u32,
    pub last_heartbeat: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub hostname: String,
    pub address: String,
    pub version: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub worker_id: String,
    pub heartbeat_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub status: String, // "idle" | "running"
    pub jobs_running: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatResponse {
    pub assignment: Option<RunAssignment>,
    /// Manager asks the worker to cancel this running run.
    pub cancel_run: Option<String>,
    pub offline_grace_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAssignment {
    pub run_id: String,
    pub job: JobSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRequest {
    pub status: String, // "success" | "failed" | "cancelled"
    pub exit_code: Option<i64>,
    pub size_before: Option<i64>,
    pub size_after: Option<i64>,
    pub bytes_transferred: Option<i64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadResponse {
    pub applied: usize,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("manager rejected the request: {0}")]
    Rejected(String),
}

pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Client {
            base: self.base.clone(),
            token: self.token.clone(),
            http: self.http.clone(),
        }
    }
}

impl Client {
    pub fn new(base: &str, token: &str) -> Result<Client, ApiError> {
        Self::build(base, token, None)
    }

    /// With a CA certificate to verify the manager's TLS (tunasync-style
    /// `ca_cert`, spec §64).
    pub fn new_with_ca(base: &str, token: &str, ca_pem: &[u8]) -> Result<Client, ApiError> {
        Self::build(base, token, Some(ca_pem))
    }

    fn build(base: &str, token: &str, ca_pem: Option<&[u8]>) -> Result<Client, ApiError> {
        let mut builder = reqwest::Client::builder();
        if let Some(pem) = ca_pem {
            let cert = reqwest::Certificate::from_pem(pem)
                .map_err(|e| ApiError::Http(e.to_string()))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder.build().map_err(|e| ApiError::Http(e.to_string()))?;
        Ok(Client {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        })
    }

    async fn send<B: Serialize>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response, ApiError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .http
            .request(method, &url)
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(b);
        }
        req.send().await.map_err(|e| ApiError::Http(e.to_string()))
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, ApiError> {
        let resp = self.send(method, path, body).await?;
        if resp.status().is_success() {
            resp.json().await.map_err(|e| ApiError::Http(e.to_string()))
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(ApiError::Rejected(text))
        }
    }

    // --- worker-facing -----------------------------------------------------

    pub async fn register_worker(&self, req: &RegisterRequest) -> Result<RegisterResponse, ApiError> {
        self.json(reqwest::Method::POST, &format!("{API_V1}/workers/register"), Some(req))
            .await
    }

    pub async fn heartbeat(
        &self,
        worker_id: &str,
        req: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ApiError> {
        self.json(
            reqwest::Method::POST,
            &format!("{API_V1}/workers/{worker_id}/heartbeat"),
            Some(req),
        )
        .await
    }

    /// Claim a run: 200 with the assignment, or Rejected (409) when it was
    /// taken by someone else.
    pub async fn claim_run(&self, run_id: &str) -> Result<Option<RunAssignment>, ApiError> {
        let resp = self
            .send(reqwest::Method::POST, &format!("{API_V1}/runs/{run_id}/claim"), None::<&()>)
            .await?;
        match resp.status().as_u16() {
            200 => Ok(Some(
                resp.json().await.map_err(|e| ApiError::Http(e.to_string()))?,
            )),
            409 => Ok(None),
            _ => Err(ApiError::Rejected(
                resp.text().await.unwrap_or_default(),
            )),
        }
    }

    pub async fn complete_run(&self, run_id: &str, req: &CompleteRequest) -> Result<(), ApiError> {
        self.json(reqwest::Method::POST, &format!("{API_V1}/runs/{run_id}/complete"), Some(req))
            .await
    }

    pub async fn unregister(&self, worker_id: &str) -> Result<(), ApiError> {
        self.json(reqwest::Method::DELETE, &format!("{API_V1}/workers/{worker_id}"), None::<&()>)
            .await
    }

    // --- operator-facing ----------------------------------------------------

    pub async fn list_jobs(&self) -> Result<Vec<JobDTO>, ApiError> {
        self.json(reqwest::Method::GET, &format!("{API_V1}/jobs"), None::<&()>)
            .await
    }

    pub async fn trigger_run(&self, job: &str) -> Result<String, ApiError> {
        self.json(reqwest::Method::POST, &format!("{API_V1}/jobs/{job}/run"), None::<&()>)
            .await
    }

    pub async fn stop_run(&self, job: &str) -> Result<(), ApiError> {
        self.json(reqwest::Method::POST, &format!("{API_V1}/jobs/{job}/stop"), None::<&()>)
            .await
    }

    pub async fn list_workers(&self) -> Result<Vec<WorkerDTO>, ApiError> {
        self.json(reqwest::Method::GET, &format!("{API_V1}/workers"), None::<&()>)
            .await
    }

    pub async fn drain_worker(&self, worker_id: &str) -> Result<(), ApiError> {
        self.json(reqwest::Method::POST, &format!("{API_V1}/workers/{worker_id}/drain"), None::<&()>)
            .await
    }

    pub async fn job_logs(&self, job: &str, tail: u32) -> Result<String, ApiError> {
        let path = format!("{API_V1}/jobs/{job}/logs?tail={tail}");
        self.json(reqwest::Method::GET, &path, None::<&()>).await
    }

    pub async fn list_proxies(&self) -> Result<serde_json::Value, ApiError> {
        self.json(reqwest::Method::GET, &format!("{API_V1}/proxies"), None::<&()>)
            .await
    }

    pub async fn job_history(&self, job: &str) -> Result<Vec<RunDTO>, ApiError> {
        self.json(reqwest::Method::GET, &format!("{API_V1}/jobs/{job}/history"), None::<&()>)
            .await
    }
}
