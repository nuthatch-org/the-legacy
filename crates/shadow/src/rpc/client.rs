//! JSON-RPC over HTTP, metered: every method attempt is charged to the budget before it is sent.
//!
//! The endpoint URL usually carries a key, so it never appears in an error or report. Transport
//! errors are reduced to their kind for the same reason.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::profile::Profile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The budget would be exceeded by this attempt; nothing was sent.
    Budget,
    /// Worth another attempt: transport failure, HTTP 429/5xx, or a provider rate limit.
    Transient(String),
    /// The provider answered and the answer is final, such as an unsupported method.
    Rpc { code: i64, message: String },
    /// The reply was not JSON-RPC.
    Malformed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Budget => write!(f, "request budget exhausted"),
            CallError::Transient(why) => write!(f, "transient: {why}"),
            CallError::Rpc { code, message } => write!(f, "JSON-RPC error {code}: {message}"),
            CallError::Malformed(why) => write!(f, "malformed reply: {why}"),
        }
    }
}

pub const METHOD_NOT_FOUND: i64 = -32601;

/// `--budget-requests` and `--budget-cu`, in logical method attempts (RFC-0002 P4).
pub struct Budget {
    requests: Option<u64>,
    cu: Option<u64>,
    cu_per_method: Option<u64>,
    attempted: AtomicU64,
    http: AtomicU64,
}

impl Budget {
    pub fn new(requests: Option<u64>, cu: Option<u64>, cu_per_method: Option<u64>) -> Self {
        Self {
            requests,
            cu,
            cu_per_method,
            attempted: AtomicU64::new(0),
            http: AtomicU64::new(0),
        }
    }

    /// Charge `methods` attempts, or nothing if any limit would be crossed.
    pub fn reserve(&self, methods: u64) -> bool {
        self.attempted
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                let next = used.checked_add(methods)?;
                if self.requests.is_some_and(|limit| next > limit) {
                    return None;
                }
                if let Some(limit) = self.cu {
                    let per = self.cu_per_method?;
                    if next.checked_mul(per)? > limit {
                        return None;
                    }
                }
                Some(next)
            })
            .is_ok()
    }

    /// Whether `methods` more attempts would fit, without charging them.
    pub fn fits(&self, methods: u64) -> bool {
        let next = self.attempted().saturating_add(methods);
        self.requests.is_none_or(|limit| next <= limit)
            && self.cu.is_none_or(|limit| {
                self.cu_per_method
                    .is_some_and(|per| next.saturating_mul(per) <= limit)
            })
    }

    pub fn attempted(&self) -> u64 {
        self.attempted.load(Ordering::SeqCst)
    }

    pub fn http_exchanges(&self) -> u64 {
        self.http.load(Ordering::SeqCst)
    }

    pub fn estimated_cu(&self) -> Option<u64> {
        self.cu_per_method
            .map(|per| self.attempted().saturating_mul(per))
    }

    pub fn limits(&self) -> (Option<u64>, Option<u64>) {
        (self.requests, self.cu)
    }
}

struct Bucket {
    rate: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn take(&mut self, n: f64) -> Duration {
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate).min(self.rate);
        self.last = now;
        self.tokens -= n;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }
}

pub struct Client {
    agent: ureq::Agent,
    url: String,
    bearer: Option<String>,
    pub budget: Budget,
    bucket: Option<Mutex<Bucket>>,
    next_id: AtomicU64,
}

impl Client {
    pub fn new(url: String, bearer: Option<String>, profile: &Profile, budget: Budget) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .http_status_as_error(false)
            .build()
            .into();
        let bucket = profile.limits.requests_per_second.map(|rate| {
            Mutex::new(Bucket {
                rate: rate.max(0.001),
                tokens: rate.max(1.0),
                last: Instant::now(),
            })
        });
        Self {
            agent,
            url,
            bearer,
            budget,
            bucket,
            next_id: AtomicU64::new(1),
        }
    }

    fn throttle(&self, methods: usize) {
        if let Some(bucket) = &self.bucket {
            let wait = bucket
                .lock()
                .expect("rate bucket poisoned")
                .take(methods as f64);
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
    }

    fn post(&self, body: &Value) -> Result<Value, CallError> {
        self.budget.http.fetch_add(1, Ordering::SeqCst);
        let text = serde_json::to_string(body).map_err(|e| CallError::Malformed(e.to_string()))?;
        let mut request = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json");
        if let Some(token) = &self.bearer {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let mut response = request
            .send(text)
            .map_err(|e| CallError::Transient(transport_kind(&e)))?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(512 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| CallError::Transient(transport_kind(&e)))?;
        if status == 429 || status >= 500 {
            return Err(CallError::Transient(format!("HTTP {status}")));
        }
        if !(200..300).contains(&status) {
            return Err(CallError::Malformed(format!("HTTP {status}")));
        }
        serde_json::from_slice(&bytes).map_err(|e| CallError::Malformed(e.to_string()))
    }

    /// One method attempt, charged before it is sent.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        if !self.budget.reserve(1) {
            return Err(CallError::Budget);
        }
        self.throttle(1);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let reply =
            self.post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        member(&reply, id)
    }

    /// A batch is charged per member, never as one request. Replies are matched by id because
    /// the spec lets a server answer in any order.
    pub fn batch(
        &self,
        calls: &[(&str, Value)],
    ) -> Result<Vec<Result<Value, CallError>>, CallError> {
        if !self.budget.reserve(calls.len() as u64) {
            return Err(CallError::Budget);
        }
        self.throttle(calls.len());
        let first = self.next_id.fetch_add(calls.len() as u64, Ordering::SeqCst);
        let body: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (method, params))| {
                json!({"jsonrpc": "2.0", "id": first + i as u64, "method": method, "params": params})
            })
            .collect();
        let reply = self.post(&Value::Array(body))?;
        let Value::Array(members) = reply else {
            // Some servers answer a whole batch with one error object.
            return Err(classify(&reply)
                .unwrap_or_else(|| CallError::Malformed("batch reply is not an array".into())));
        };
        Ok((0..calls.len() as u64)
            .map(|i| {
                members
                    .iter()
                    .find(|m| m.get("id").and_then(Value::as_u64) == Some(first + i))
                    .ok_or_else(|| CallError::Transient(format!("batch reply lacks member {i}")))
                    .and_then(|m| member(m, first + i))
            })
            .collect())
    }
}

fn member(reply: &Value, id: u64) -> Result<Value, CallError> {
    if reply.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(CallError::Malformed(format!("reply id is not {id}")));
    }
    if let Some(error) = classify(reply) {
        return Err(error);
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| CallError::Malformed("reply has neither result nor error".into()))
}

fn classify(reply: &Value) -> Option<CallError> {
    let error = reply.get("error")?;
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();
    // -32005 is the de facto "limit exceeded"; -32603 is how several providers report overload.
    Some(
        if matches!(code, -32005 | -32603) || message.contains("rate") {
            CallError::Transient(format!("JSON-RPC error {code}: {message}"))
        } else {
            CallError::Rpc { code, message }
        },
    )
}

fn transport_kind(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Timeout(_) => "timeout".into(),
        ureq::Error::Io(e) => format!("io {:?}", e.kind()),
        ureq::Error::ConnectionFailed => "connection failed".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::BodyExceedsLimit(n) => format!("body over {n} bytes"),
        _ => "transport error".into(),
    }
}
