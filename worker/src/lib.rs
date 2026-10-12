mod api;
mod auth;
mod db;
mod detail;
mod http;
mod icons;
mod page;
mod probe;
mod range;
mod reads;
mod tick;

use worker::{event, Context, Env, Request, Response, Result, ScheduledEvent};

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    http::handle(req, env).await
}

#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: worker::ScheduleContext) {
    if let Err(e) = tick::tick(env).await {
        worker::console_log!("cron failed: {e}");
    }
}
