//! The public API is described in openapi.yaml (served at /v1/openapi.yaml). The internal one, on
//! a unix socket for the BMCs, is:
//!   - POST /internal/leases/{id}/power/{on,off,reset,cycle,status}: the IPMI responders;
//!   - POST /internal/leases/{id}/boot (the bf.cfg) and /internal/leases/{id}/reboot: mockbmc.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State},
    http::{StatusCode, header::CONTENT_TYPE},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::{
    bmc::HostPowerAction,
    lease::{Lease, Level, Mode},
    manager::{CreateRequest, LeaseManager, Refusal},
    vm::VmPowerAction,
};

const OPENAPI_SPEC: &[u8] = include_bytes!("../openapi.yaml");
/// The biggest bf.cfg dpusimaas takes.
const MAX_BFCFG_BYTES: usize = 64 << 20;

type Manager = State<Arc<LeaseManager>>;

pub fn public_router(manager: Arc<LeaseManager>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/v1/openapi.yaml", get(openapi_spec))
        .route("/v1/info", get(info))
        .route("/v1/dpus", get(list_leases).post(create_lease))
        .route("/v1/dpus/{id}", get(get_lease).delete(delete_lease))
        .route("/v1/dpus/{id}/renew", post(renew_lease))
        .route(
            "/v1/dpus/{id}/boot",
            post(boot_host_trusted).layer(DefaultBodyLimit::max(MAX_BFCFG_BYTES)),
        )
        .route("/v1/dpus/{id}/reset", post(reset_dpu))
        .layer(middleware::from_fn(log_request))
        .with_state(manager)
}

pub fn internal_router(manager: Arc<LeaseManager>) -> Router {
    Router::new()
        .route("/internal/leases/{id}/power/{action}", post(host_power))
        .route(
            "/internal/leases/{id}/boot",
            post(dpu_installed).layer(DefaultBodyLimit::max(MAX_BFCFG_BYTES)),
        )
        .route("/internal/leases/{id}/reboot", post(dpu_reset))
        .with_state(manager)
}

/// An error, answered as `{"error": ...}` with the status the kind of error calls for.
struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> ApiError {
        ApiError(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.downcast_ref::<Refusal>() {
            Some(Refusal::NotFound) => StatusCode::NOT_FOUND,
            Some(Refusal::Full(_)) => StatusCode::SERVICE_UNAVAILABLE,
            Some(Refusal::Invalid(_)) => StatusCode::BAD_REQUEST,
            Some(Refusal::Conflict(_)) => StatusCode::CONFLICT,
            None => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("{:#}", self.0);
        }
        let message = format!("{:#}", self.0);
        pretty_json(status, &BTreeMap::from([("error", message)]))
    }
}

type ApiResult = Result<Response, ApiError>;

/// JSON, indented, as the Go dpusimaas wrote it.
fn pretty_json(status: StatusCode, body: &impl Serialize) -> Response {
    match serde_json::to_string_pretty(body) {
        Ok(serialized) => (
            status,
            [(CONTENT_TYPE, "application/json")],
            format!("{serialized}\n"),
        )
            .into_response(),
        Err(error) => {
            tracing::error!("serializing a response: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn log_request(
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() != "/healthz" {
        tracing::info!(
            method = %request.method(),
            path = request.uri().path(),
            from = %client,
            "request"
        );
    }
    next.run(request).await
}

async fn openapi_spec() -> Response {
    ([(CONTENT_TYPE, "application/yaml")], OPENAPI_SPEC).into_response()
}

/// Fields in alphabetical order, as the Go dpusimaas wrote them.
#[derive(Serialize)]
struct ServiceInfo {
    leased: usize,
    levels: BTreeMap<&'static str, &'static str>,
    max_dpus: usize,
    modes: [Mode; 2],
}

async fn info(State(manager): Manager) -> Response {
    let levels = Level::ALL
        .into_iter()
        .map(|level| (level.name(), level.description()))
        .collect();
    pretty_json(
        StatusCode::OK,
        &ServiceInfo {
            max_dpus: manager.config.max_leases,
            leased: manager.store.count(),
            modes: Mode::ALL,
            levels,
        },
    )
}

#[derive(Deserialize)]
struct LeaseFilter {
    owner: Option<String>,
    /// The DPU's serial, as its host reads it from its PFs' VPD.
    serial: Option<String>,
}

async fn list_leases(State(manager): Manager, Query(filter): Query<LeaseFilter>) -> Response {
    let owner = filter.owner.filter(|owner| !owner.is_empty());
    let serial = filter.serial.filter(|serial| !serial.is_empty());
    let leases: Vec<Lease> = manager
        .store
        .list()
        .into_iter()
        .filter(|lease| owner.as_ref().is_none_or(|owner| &lease.owner == owner))
        .filter(|lease| {
            serial
                .as_ref()
                .is_none_or(|serial| lease.serial.eq_ignore_ascii_case(serial))
        })
        .collect();
    pretty_json(StatusCode::OK, &leases)
}

async fn create_lease(State(manager): Manager, body: Bytes) -> ApiResult {
    let request: CreateRequest =
        serde_json::from_slice(&body).map_err(|error| Refusal::Invalid(error.to_string()))?;
    let lease = manager.create(request).await?;
    Ok(pretty_json(StatusCode::CREATED, &lease))
}

fn existing_lease(manager: &LeaseManager, lease_id: &str) -> Result<Lease, Refusal> {
    manager.store.get(lease_id).ok_or(Refusal::NotFound)
}

async fn get_lease(State(manager): Manager, Path(lease_id): Path<String>) -> ApiResult {
    Ok(pretty_json(
        StatusCode::OK,
        &existing_lease(&manager, &lease_id)?,
    ))
}

async fn delete_lease(State(manager): Manager, Path(lease_id): Path<String>) -> ApiResult {
    manager.destroy(&lease_id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RenewRequest {
    ttl_hours: Option<i64>,
}

async fn renew_lease(
    State(manager): Manager,
    Path(lease_id): Path<String>,
    body: Bytes,
) -> ApiResult {
    // A body that isn't a renew request (or none) means ttl_hours 0: the lease no longer ends.
    let request: RenewRequest = serde_json::from_slice(&body).unwrap_or_default();
    let lease = manager.renew(&lease_id, request.ttl_hours.unwrap_or_default())?;
    Ok(pretty_json(StatusCode::OK, &lease))
}

#[derive(Deserialize)]
struct BootQuery {
    host_node: Option<String>,
}

/// A host-trusted lease's host handing over the bf.cfg DPF generated, and its node name.
async fn boot_host_trusted(
    State(manager): Manager,
    Path(lease_id): Path<String>,
    Query(query): Query<BootQuery>,
    bfcfg: Bytes,
) -> ApiResult {
    let lease = existing_lease(&manager, &lease_id)?;
    if lease.mode != Mode::HostTrusted {
        return Err(Refusal::Conflict(
            "a zero-trust DPU boots when DPF installs a BFB through its BMC".to_owned(),
        )
        .into());
    }
    if bfcfg.is_empty() {
        return Err(Refusal::Invalid("the request body must be the bf.cfg".to_owned()).into());
    }
    let host_node = query.host_node.filter(|node| !node.is_empty());
    manager.boot(&lease_id, &bfcfg, host_node).await?;
    Ok(pretty_json(
        StatusCode::OK,
        &existing_lease(&manager, &lease_id)?,
    ))
}

async fn reset_dpu(State(manager): Manager, Path(lease_id): Path<String>) -> ApiResult {
    existing_lease(&manager, &lease_id)?;
    manager
        .vm_action(&lease_id, "reset through the API", VmPowerAction::Reset)
        .await;
    Ok(pretty_json(
        StatusCode::OK,
        &existing_lease(&manager, &lease_id)?,
    ))
}

#[derive(Serialize)]
struct PowerResult {
    result: &'static str,
}

async fn host_power(
    State(manager): Manager,
    Path((lease_id, action)): Path<(String, String)>,
) -> ApiResult {
    let action = HostPowerAction::parse(&action)?;
    let result = manager.host_power(&lease_id, action).await?;
    Ok(pretty_json(StatusCode::OK, &PowerResult { result }))
}

async fn dpu_installed(
    State(manager): Manager,
    Path(lease_id): Path<String>,
    bfcfg: Bytes,
) -> ApiResult {
    if bfcfg.is_empty() {
        return Err(Refusal::Invalid("the request body must be the bf.cfg".to_owned()).into());
    }
    manager
        .dpu_installed(&lease_id, &bfcfg)
        .await
        .with_context(|| format!("booting the DPU VM of lease {lease_id}"))?;
    Ok(pretty_json(StatusCode::OK, &PowerResult { result: "ok" }))
}

async fn dpu_reset(State(manager): Manager, Path(lease_id): Path<String>) -> ApiResult {
    manager.dpu_reset(&lease_id).await?;
    Ok(pretty_json(StatusCode::OK, &PowerResult { result: "ok" }))
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;
    use crate::lease::LeaseState;

    /// The states a lease can be in, as the API names them.
    const STATE_NAMES: [(LeaseState, &str); 6] = [
        (LeaseState::Starting, "starting"),
        (LeaseState::Waiting, "waiting"),
        (LeaseState::Booting, "booting"),
        (LeaseState::Running, "running"),
        (LeaseState::PoweredOff, "powered-off"),
        (LeaseState::Failed, "failed"),
    ];

    #[test]
    fn states_serialize_as_the_spec_names_them() -> anyhow::Result<()> {
        let spec = String::from_utf8_lossy(OPENAPI_SPEC);
        let spec_states = spec
            .lines()
            .find(|line| line.contains("enum: [starting"))
            .ok_or_else(|| anyhow!("the spec lists no states"))?;
        for (state, name) in STATE_NAMES {
            assert_eq!(serde_json::to_value(state)?, serde_json::json!(name));
            assert!(spec_states.contains(name), "{name} isn't in {spec_states}");
        }
        Ok(())
    }
}
