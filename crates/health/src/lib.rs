//! health — HTTP health endpoint + startup self-check for gate-arb.
//!
//! Exposes `/health` GET returning JSON:
//! ```json
//! {
//!   "status": "ok" | "degraded" | "unhealthy",
//!   "gate_ws": "connected" | "disconnected" | "unknown",
//!   "frontend_ws": "running" | "stopped" | "unknown",
//!   "engine": "processing" | "idle" | "unknown",
//!   "startup_ok": true | false,
//!   "uptime_s": 12345
//! }
//! ```

use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{info, warn};

/// Gate.io TCP endpoint for connectivity check.
const GATEIO_HOST: &str = "api.gateio.ws:443";

/// Shared health state — written by gateway/frontend threads, read by HTTP handler.
pub struct HealthState {
    pub gate_ws: GateWsStatus,
    pub frontend_ws: FrontendStatus,
    pub engine: EngineStatus,
    pub startup_ok: bool,
    /// Startup self-check error message if any.
    pub startup_error: Option<String>,
    /// When the process started.
    pub start_time: Instant,
}

impl Default for HealthState {
    fn default() -> Self {
        Self {
            gate_ws: GateWsStatus::Unknown,
            frontend_ws: FrontendStatus::Unknown,
            engine: EngineStatus::Unknown,
            startup_ok: false,
            startup_error: None,
            start_time: Instant::now(),
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateWsStatus {
    #[default]
    Unknown,
    Connected,
    Disconnected,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendStatus {
    #[default]
    Unknown,
    Running,
    Stopped,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStatus {
    #[default]
    Unknown,
    Processing,
    Idle,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub gate_ws: String,
    pub frontend_ws: String,
    pub engine: String,
    pub startup_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<String>,
    pub uptime_s: u64,
}

impl HealthState {
    pub fn overall_status(&self) -> &'static str {
        if !self.startup_ok {
            return "unhealthy";
        }
        let ws_ok = self.gate_ws == GateWsStatus::Connected;
        let fe_ok = self.frontend_ws == FrontendStatus::Running;

        if ws_ok && fe_ok {
            "ok"
        } else if ws_ok || fe_ok {
            "degraded"
        } else {
            "unhealthy"
        }
    }

    pub fn to_response(&self) -> HealthResponse {
        HealthResponse {
            status: self.overall_status().to_string(),
            gate_ws: format!("{:?}", self.gate_ws).to_lowercase(),
            frontend_ws: format!("{:?}", self.frontend_ws).to_lowercase(),
            engine: format!("{:?}", self.engine).to_lowercase(),
            startup_ok: self.startup_ok,
            startup_error: self.startup_error.clone(),
            uptime_s: self.start_time.elapsed().as_secs(),
        }
    }
}

/// Handle to the health server and state — cloneable.
#[derive(Clone)]
pub struct HealthHandle {
    pub state: Arc<RwLock<HealthState>>,
    port: u16,
}

impl Default for HealthHandle {
    fn default() -> Self {
        Self::new(8081)
    }
}

impl HealthHandle {
    /// Create a new health handle.
    pub fn new(port: u16) -> Self {
        Self {
            state: Arc::new(RwLock::new(HealthState {
                start_time: Instant::now(),
                ..Default::default()
            })),
            port,
        }
    }

    /// Run startup self-check: verify Gate.io REST API is reachable via TCP.
    /// Sets startup_ok = true on success, or false with error message.
    pub async fn run_startup_check(&self) {
        let result = timeout(Duration::from_secs(5), TcpStream::connect(GATEIO_HOST)).await;

        match result {
            Ok(Ok(_stream)) => {
                let mut s = self.state.write().await;
                s.startup_ok = true;
                s.startup_error = None;
                info!("Health startup check PASSED: Gate.io reachable on TCP:443");
            }
            Ok(Err(e)) => {
                let mut s = self.state.write().await;
                s.startup_ok = false;
                s.startup_error = Some(format!("TCP connection failed: {}", e));
                warn!("Health startup check FAILED: {}", e);
            }
            Err(_) => {
                let mut s = self.state.write().await;
                s.startup_ok = false;
                s.startup_error = Some("TCP connection timed out (>5s)".into());
                warn!("Health startup check FAILED: connection timed out");
            }
        }
    }

    /// Update gate WS status.
    pub async fn set_gate_ws(&self, status: GateWsStatus) {
        self.state.write().await.gate_ws = status;
    }

    /// Update frontend WS status.
    pub async fn set_frontend_ws(&self, status: FrontendStatus) {
        self.state.write().await.frontend_ws = status;
    }

    /// Update engine status.
    pub async fn set_engine(&self, status: EngineStatus) {
        self.state.write().await.engine = status;
    }

    /// Start the HTTP health server. Runs forever.
    pub async fn run_server(&self) {
        let state = Arc::clone(&self.state);
        let port = self.port;

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("Health server failed to bind port {}: {}", port, e);
                    return;
                }
            };
            info!("Health server listening on port {}", port);

            loop {
                match listener.accept().await {
                    Ok((mut stream, _)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            let response = {
                                let s = state.read().await;
                                let resp = s.to_response();
                                serde_json::to_string(&resp)
                                    .unwrap_or_else(|_| r#"{"status":"error"}"#.to_string())
                            };

                            let body_len = response.len();
                            let reply = format!(
                                "HTTP/1.1 200 OK\r\n\
                                Content-Type: application/json\r\n\
                                Content-Length: {}\r\n\
                                Connection: close\r\n\
                                \r\n\
                                {}",
                                body_len, response
                            );

                            use tokio::io::AsyncWriteExt;
                            let _ = stream.write_all(reply.as_bytes()).await;
                        });
                    }
                    Err(e) => {
                        warn!("Health server accept error: {}", e);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_state_overall() {
        let state = HealthState::default();
        assert_eq!(state.overall_status(), "unhealthy"); // no startup

        let mut s = HealthState::default();
        s.startup_ok = true;
        s.gate_ws = GateWsStatus::Connected;
        s.frontend_ws = FrontendStatus::Running;
        assert_eq!(s.overall_status(), "ok");

        let mut s = HealthState::default();
        s.startup_ok = true;
        s.gate_ws = GateWsStatus::Disconnected;
        s.frontend_ws = FrontendStatus::Running;
        assert_eq!(s.overall_status(), "degraded");
    }
}
