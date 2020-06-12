//! HTTP API delivering JSON over `RESTish` endpoints.

use librad::paths;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::RwLock;
use warp::{path, Filter, Rejection, Reply};

use crate::registry;

mod avatar;
mod control;
mod doc;
mod error;
mod identity;
mod notification;
mod org;
mod project;
mod session;
mod source;
mod transaction;
mod user;

/// Main entry point for HTTP API.
pub fn api<R>(
    librad_paths: paths::Paths,
    registry: R,
    store: kv::Store,
    enable_control: bool,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone
where
    R: registry::Cache + registry::Client + 'static,
{
    let librad_paths = Arc::new(RwLock::new(librad_paths));
    let registry = Arc::new(RwLock::new(registry));
    let store = Arc::new(RwLock::new(store));
    let subscriptions = crate::notification::Subscriptions::default();

    let api = path("v1").and(
        avatar::get_filter()
            .or(control::routes(
                enable_control,
                Arc::clone(&librad_paths),
                Arc::clone(&registry),
            ))
            .or(identity::filters(Arc::clone(&registry), Arc::clone(&store)))
            .or(notification::filters(subscriptions.clone()))
            .or(org::routes(
                Arc::clone(&librad_paths),
                Arc::clone(&registry),
                subscriptions.clone(),
            ))
            .or(project::filters(
                Arc::clone(&librad_paths),
                Arc::clone(&registry),
                subscriptions.clone(),
            ))
            .or(session::routes(Arc::clone(&registry), Arc::clone(&store)))
            .or(source::routes(librad_paths))
            .or(transaction::filters(Arc::clone(&registry)))
            .or(user::routes(registry, store, subscriptions)),
    );
    // let docs = path("docs").and(doc::filters(&api));
    let docs = path("docs").and(doc::index_filter().or(doc::describe_filter(&api)));
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(&[warp::http::header::CONTENT_TYPE])
        .allow_methods(&[
            warp::http::Method::DELETE,
            warp::http::Method::GET,
            warp::http::Method::POST,
            warp::http::Method::OPTIONS,
        ]);
    let log = warp::log::custom(|info| {
        log::info!(
            target: "proxy::http",
            "\"{} {} {:?}\" {} {:?}",
            info.method(),
            info.path(),
            info.version(),
            info.status().as_u16(),
            info.elapsed(),
        );
    });

    let recovered = api.or(docs).recover(error::recover);

    recovered.with(cors).with(log)
}

/// State filter to expose the [`librad::paths::Paths`] to handlers.
#[must_use]
pub fn with_paths(
    paths: Arc<RwLock<paths::Paths>>,
) -> impl Filter<Extract = (Arc<RwLock<paths::Paths>>,), Error = Infallible> + Clone {
    warp::any().map(move || Arc::clone(&paths))
}

/// Thread-safe container for threadsafe pass-through to filters and handlers.
pub type Shared<T> = Arc<RwLock<T>>;

/// State filter to expose a [`Shared`] and its content.
#[must_use]
pub fn with_shared<T>(
    container: Shared<T>,
) -> impl Filter<Extract = (Shared<T>,), Error = Infallible> + Clone
where
    T: Send + Sync,
{
    warp::any().map(move || Arc::clone(&container))
}

/// State filter to expose [`kv::Store`] to handlers.
#[must_use]
pub fn with_store(
    store: Arc<RwLock<kv::Store>>,
) -> impl Filter<Extract = (Arc<RwLock<kv::Store>>,), Error = Infallible> + Clone {
    warp::any().map(move || Arc::clone(&store))
}

/// State filter to expose [`notification::Subscriptions`] to handlers.
#[must_use]
pub fn with_subscriptions(
    subscriptions: crate::notification::Subscriptions,
) -> impl Filter<Extract = (crate::notification::Subscriptions,), Error = Infallible> + Clone {
    warp::any().map(move || crate::notification::Subscriptions::clone(&subscriptions))
}
