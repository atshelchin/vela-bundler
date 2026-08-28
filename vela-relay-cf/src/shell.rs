//! Entry points of the Cloudflare shell.

use worker::{Context, Env, Request, Response, Result, event};

#[event(fetch)]
pub async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    crate::http::handle(req, env).await
}
