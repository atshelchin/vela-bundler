//! Entry points (fetch / queue / scheduled) and the module tree. T001 stub:
//! the fetch handler answers 501 until US1 lands.

use worker::{Context, Env, Request, Response, Result, event};

#[event(fetch)]
pub async fn fetch(_req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    Response::error("not implemented", 501)
}
