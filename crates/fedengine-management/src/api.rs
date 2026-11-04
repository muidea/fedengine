use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use fedengine_contract::*;
use fedengine_core::*;
use modorch::{prelude::*, *};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct ManagementListenerStatus {
    address: Mutex<Option<SocketAddr>>,
}
impl ManagementListenerStatus {
    pub fn address(&self) -> Option<SocketAddr> {
        *self.address.lock().unwrap()
    }
}
pub struct ManagementApiFactory {
    pub config: Arc<Config>,
    pub status: Arc<ManagementListenerStatus>,
}
struct ApiInitiator {
    config: Arc<Config>,
    status: Arc<ManagementListenerStatus>,
    listener: Option<TcpListener>,
    stop: CancellationToken,
}
fn descriptor() -> ComponentDescriptor {
    ComponentDescriptor::new(ComponentRef::initiator("management-api"))
}
impl InitiatorFactory for ManagementApiFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(ApiInitiator {
            config: self.config.clone(),
            status: self.status.clone(),
            listener: None,
            stop: CancellationToken::new(),
        }))
    }
}
#[async_trait]
impl Lifecycle for ApiInitiator {
    async fn setup(&mut self, _: &SetupContext) -> FrameworkResult<()> {
        let listener = TcpListener::bind(&self.config.management.listen)
            .await
            .map_err(|_| FrameworkError::InvalidConfig("cannot bind management listener".into()))?;
        *self.status.address.lock().unwrap() = Some(listener.local_addr().map_err(|_| {
            FrameworkError::InvalidConfig("cannot read management listener address".into())
        })?);
        self.listener = Some(listener);
        Ok(())
    }
    async fn begin_shutdown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.stop.cancel();
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.listener.take();
        *self.status.address.lock().unwrap() = None;
        Ok(())
    }
}
#[derive(Clone)]
struct ApiState {
    hub: ExternalEventHub,
    timeout: Duration,
}
#[async_trait]
impl Initiator for ApiInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    async fn activate_ingress(&mut self, ctx: &RuntimeContext) -> FrameworkResult<()> {
        let listener = self.listener.take().unwrap();
        let state = ApiState {
            hub: ctx.event_hub.external(),
            timeout: Duration::from_millis(self.config.management.request_timeout_ms),
        };
        let router = Router::new()
            .route("/", get(crate::web::index))
            .route("/manage", get(crate::web::index))
            .route("/manage/", get(crate::web::index))
            .route("/manage/app.js", get(crate::web::javascript))
            .route("/manage/style.css", get(crate::web::stylesheet))
            .route("/api/v1/{*path}", any(dispatch))
            .layer(DefaultBodyLimit::max(65_536))
            .with_state(state);
        let stop = self.stop.clone();
        ctx.tasks.spawn(move |shutdown| async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    tokio::select! { _ = stop.cancelled() => {}, _ = shutdown.cancelled() => {} }
                })
                .await
                .map_err(|_| {
                    FrameworkError::InvalidConfig("management HTTP server failed".into())
                })?;
            Ok(())
        })?;
        Ok(())
    }
}
async fn dispatch(
    State(state): State<ApiState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method != Method::GET && method != Method::HEAD && !crate::web::same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error":"cross-origin management writes are not allowed"})),
        )
            .into_response();
    }
    let (request, writing) = match decode(&method, &uri, &headers, &body) {
        Ok(request) => request,
        Err(error) => return error_response(error),
    };
    let catalog = if let ManagementRequest::Catalog { id, .. } = &request {
        match axum::extract::Query::<CatalogParameters>::try_from_uri(&uri)
            .map_err(|_| EngineError::config("invalid catalog query parameters"))
            .and_then(|q| q.0.resolve(&id.0))
        {
            Ok(q) => Some((id.0.clone(), q)),
            Err(e) => return error_response(e),
        }
    } else {
        None
    };
    let ticket = match state.hub.request::<_, Result<ManagementReply>>(
        MANAGEMENT_TOPIC,
        Destination::Component(ComponentRef::module("source-manager")),
        request,
        Default::default(),
    ) {
        Ok(ticket) => ticket,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"error":"management admission unavailable"})),
            )
                .into_response();
        }
    };
    match ticket.wait(state.timeout).await {
        Ok(result) => match result.as_ref() {
            Ok(ManagementReply::Catalog(page)) if catalog.is_some() => {
                let (id,q)=catalog.as_ref().unwrap();
                match q.response(id,page) {Ok(value)=>(StatusCode::OK,axum::Json(value)).into_response(),Err(e)=>error_response(e)}
            },
            Ok(reply) => (if writing { StatusCode::ACCEPTED } else { StatusCode::OK }, axum::Json(reply)).into_response(),
            Err(error) => error_response(error.clone()),
        },
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"error":"request outcome is not confirmed; retry using the same Idempotency-Key or read the operation"}))).into_response(),
    }
}
fn error_response(error: EngineError) -> Response {
    let status = match error.sqlstate {
        "22023" => StatusCode::BAD_REQUEST,
        "42704" | "3D000" => StatusCode::NOT_FOUND,
        "40001" | "55000" | "55P03" => StatusCode::CONFLICT,
        "53300" | "54000" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        axum::Json(json!({"error":error.message,"code":error.sqlstate})),
    )
        .into_response()
}
use crate::pagination::CatalogParameters;
fn decode(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(ManagementRequest, bool)> {
    let parts: Vec<_> = uri
        .path()
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let route = parts.get(2..).unwrap_or_default();
    if *method == Method::GET {
        let request = match route {
            ["virtual-databases"] => ManagementRequest::VirtualDatabases,
            ["virtual-databases", name] => ManagementRequest::VirtualDatabase((*name).into()),
            ["sources"] => ManagementRequest::List,
            ["sources", id] => ManagementRequest::Get(DataSourceId((*id).into())),
            ["operations", id] => ManagementRequest::Operation((*id).into()),
            ["sources", id, "catalog"] => {
                let query = axum::extract::Query::<CatalogParameters>::try_from_uri(uri)
                    .map_err(|_| EngineError::config("invalid catalog query parameters"))?
                    .0
                    .resolve(id)?;
                ManagementRequest::Catalog {
                    object: query.object,
                    snapshot_id: query.snapshot_id,
                    id: DataSourceId((*id).into()),
                    schema: query.schema,
                    object_type: query.object_type,
                    offset: query.offset.unwrap_or(0),
                    limit: query.limit.unwrap_or(100),
                }
            }
            _ => return Err(EngineError::new("42704", "management route does not exist")),
        };
        return Ok((request, false));
    }
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| EngineError::config("Idempotency-Key header is required"))?
        .to_owned();
    let mut value: Value = serde_json::from_slice(body)
        .map_err(|_| EngineError::config("body must be a JSON object"))?;
    if route.first() == Some(&"virtual-databases") {
        let mutation = match (method.as_str(), route) {
            ("POST", ["virtual-databases"]) => VirtualDatabaseMutation::Create {
                definition: serde_json::from_value(value)
                    .map_err(|_| EngineError::config("invalid virtual database definition"))?,
            },
            (method, ["virtual-databases", name]) if method == "PUT" || method == "DELETE" => {
                let object = value
                    .as_object_mut()
                    .ok_or_else(|| EngineError::config("body must be a JSON object"))?;
                // Identity is supplied by the route, never by an untrusted body override.
                if object.contains_key("name") || object.contains_key("action") {
                    return Err(EngineError::config(
                        "name and action are supplied by the route",
                    ));
                }
                object.insert("name".into(), json!(name));
                object.insert(
                    "action".into(),
                    json!(if method == "PUT" { "replace" } else { "delete" }),
                );
                serde_json::from_value(value).map_err(|_| {
                    EngineError::config("invalid virtual database mutation or target")
                })?
            }
            _ => return Err(EngineError::new("42704", "management route does not exist")),
        };
        return Ok((
            ManagementRequest::MutateVirtualDatabase {
                mutation,
                idempotency_key: key,
            },
            false,
        ));
    }
    let action = match (method.as_str(), route) {
        ("POST", ["sources"]) => ManagementAction::Register {
            definition: serde_json::from_value(value)
                .map_err(|_| EngineError::config("invalid source definition"))?,
        },
        (method, ["sources", id]) if method == "PATCH" || method == "DELETE" => {
            let object = value
                .as_object_mut()
                .ok_or_else(|| EngineError::config("body must be a JSON object"))?;
            object.insert(
                "action".into(),
                json!(if method == "PATCH" { "patch" } else { "delete" }),
            );
            object.insert("id".into(), json!(id));
            serde_json::from_value(value)
                .map_err(|_| EngineError::config("invalid management action or target revision"))?
        }
        (method, ["sources", id, action])
            if (*action == "schemas" && method == "PUT")
                || (["apply", "disable", "refresh", "probe", "discover"].contains(action)
                    && method == "POST") =>
        {
            let object = value
                .as_object_mut()
                .ok_or_else(|| EngineError::config("body must be a JSON object"))?;
            if *action == "schemas" && !object.contains_key("default_schema") {
                return Err(EngineError::config(
                    "schemas requires default_schema, including null to clear it",
                ));
            }
            object.insert("action".into(), json!(action));
            object.insert("id".into(), json!(id));
            serde_json::from_value(value)
                .map_err(|_| EngineError::config("invalid management action or target revision"))?
        }
        _ => return Err(EngineError::new("42704", "management route does not exist")),
    };
    Ok((
        ManagementRequest::Submit {
            action,
            idempotency_key: key,
        },
        true,
    ))
}
