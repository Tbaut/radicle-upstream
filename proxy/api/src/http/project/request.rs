//! Endpoints for project search requests.

use warp::{filters::BoxedFilter, path, Filter, Rejection, Reply};

use crate::{context, http};

/// Combination of all routes.
pub fn filters(ctx: context::Context) -> BoxedFilter<(impl Reply,)> {
    cancel_filter(ctx.clone())
        .or(create_filter(ctx.clone()))
        .or(list_filter(ctx))
        .boxed()
}

/// `DELETE /<urn>`
fn cancel_filter(
    ctx: context::Context,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    http::with_context_unsealed(ctx)
        .and(warp::delete())
        .and(path::param::<coco::Urn>())
        .and(path::end())
        .and_then(handler::cancel)
}

/// `PUT /<urn>`
fn create_filter(
    ctx: context::Context,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    http::with_context_unsealed(ctx)
        .and(warp::put())
        .and(path::param::<coco::Urn>())
        .and(path::end())
        .and_then(handler::create)
}

/// `GET /`
fn list_filter(
    ctx: context::Context,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    http::with_context_unsealed(ctx)
        .and(warp::get())
        .and(path::end())
        .and_then(handler::list)
}

/// Request handlers for initiating searches for projects on the network.
mod handler {
    use std::time::Instant;

    use warp::{http::StatusCode, reply, Rejection, Reply};

    use crate::{context, error};

    /// Abort search for an ongoing request.
    pub async fn cancel(
        mut ctx: context::Unsealed,
        urn: coco::Urn,
    ) -> Result<impl Reply, Rejection> {
        ctx.peer_control
            .cancel_project_request(&urn, Instant::now())
            .await
            .map_err(error::Error::from)?;

        Ok(reply::with_status(reply(), StatusCode::NO_CONTENT))
    }

    /// Kick off a network request for the [`crate::project::Project`] of the given `id`.
    ///
    /// FIXME(xla): Endpoint ought to return `201` if the request was newly created, otherwise
    /// `200` if there was a request present for the urn.
    pub async fn create(
        mut ctx: context::Unsealed,
        urn: coco::Urn,
    ) -> Result<impl Reply, Rejection> {
        let request = ctx.peer_control.request_project(&urn, Instant::now()).await;

        Ok(reply::json(&request))
    }

    /// List all project requests the current user has issued.
    pub async fn list(mut ctx: context::Unsealed) -> Result<impl Reply, Rejection> {
        let requests = ctx.peer_control.get_project_requests().await;

        Ok(reply::json(&requests))
    }
}

#[cfg(test)]
mod test {
    use std::time::Instant;

    use pretty_assertions::assert_eq;
    use serde_json::json;
    use warp::{http::StatusCode, test::request};

    use crate::{context, http};

    #[tokio::test]
    async fn cancel() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir()?;
        let mut ctx = context::Unsealed::tmp(&tmp_dir).await?;
        let api = super::filters(ctx.clone().into());

        let urn = coco::Urn::new(
            coco::Hash::hash(b"kisses-of-the-sun"),
            coco::uri::Protocol::Git,
            coco::uri::Path::empty(),
        );

        let _request = ctx.peer_control.request_project(&urn, Instant::now()).await;
        let res = request()
            .method("DELETE")
            .path(&format!("/{}", urn))
            .reply(&api)
            .await;

        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        Ok(())
    }

    #[tokio::test]
    async fn create() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir()?;
        let mut ctx = context::Unsealed::tmp(&tmp_dir).await?;
        let api = super::filters(ctx.clone().into());

        let urn = coco::Urn::new(
            coco::Hash::hash(b"kisses-of-the-sun"),
            coco::uri::Protocol::Git,
            coco::uri::Path::empty(),
        );

        let res = request()
            .method("PUT")
            .path(&format!("/{}", urn))
            .reply(&api)
            .await;
        let want = ctx.peer_control.get_project_request(&urn).await;

        http::test::assert_response(&res, StatusCode::OK, |have| {
            assert_eq!(have, json!(want));
        });

        Ok(())
    }

    #[tokio::test]
    async fn list() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir()?;
        let mut ctx = context::Unsealed::tmp(&tmp_dir).await?;
        let api = super::filters(ctx.clone().into());

        let urn = coco::Urn::new(
            coco::Hash::hash(b"kisses-of-the-sun"),
            coco::uri::Protocol::Git,
            coco::uri::Path::empty(),
        );

        let want = ctx.peer_control.request_project(&urn, Instant::now()).await;
        let res = request().method("GET").path("/").reply(&api).await;

        http::test::assert_response(&res, StatusCode::OK, |have| {
            assert_eq!(have, json!([want]));
        });

        Ok(())
    }
}
