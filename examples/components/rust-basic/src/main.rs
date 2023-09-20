use anyhow::Result;
use wasm_workers_rs::{
    http::{self, Request, Response},
    worker, Content,
};

#[worker]
fn handler(_req: Request<String>) -> Result<Response<Content>> {
    Ok(http::Response::builder()
        .status(200)
        .header("x-generated-by", "wasm-workers-server")
        .body(String::from("Hello, world!").into())?)
}
