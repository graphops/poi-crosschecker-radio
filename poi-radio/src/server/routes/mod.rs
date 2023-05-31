use async_graphql::http::{playground_source, GraphQLPlaygroundConfig};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{
    extract::Extension,
    http::StatusCode,
    response::{Html, IntoResponse},
    Json,
};
use opentelemetry::trace::TraceContextExt;
use serde::Serialize;
use std::sync::Arc;
use tracing::{span, trace, Instrument, Level};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use super::model::POIRadioContext;
use crate::server::model::POIRadioSchema;

#[derive(Serialize)]
struct Health {
    healthy: bool,
}

pub(crate) async fn health() -> impl IntoResponse {
    let health = Health { healthy: true };

    (StatusCode::OK, Json(health))
}

pub(crate) async fn graphql_playground() -> impl IntoResponse {
    Html(playground_source(
        GraphQLPlaygroundConfig::new("/").subscription_endpoint("/ws"),
    ))
}

pub(crate) async fn graphql_handler(
    req: GraphQLRequest,
    Extension(schema): Extension<POIRadioSchema>,
    Extension(context): Extension<Arc<POIRadioContext>>,
) -> GraphQLResponse {
    let span = span!(Level::TRACE, "graphql_execution");

    trace!("Processing GraphQL request");

    let response = async move { schema.execute(req.into_inner().data(context)).await }
        .instrument(span.clone())
        .await;

    trace!("Processing GraphQL request finished");

    response
        .extension(
            "traceId",
            async_graphql::Value::String(format!(
                "{}",
                span.context().span().span_context().trace_id()
            )),
        )
        .into()
}
