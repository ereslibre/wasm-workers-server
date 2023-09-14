// Copyright 2023 VMware, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{features::http_requests::HttpRequestsConfig, WORKERS};
use actix_web::http::Uri;
use reqwest::Method;
use std::collections::HashMap;
use tokio::runtime::Builder;

// Implement the HTTP bindings for the workers.
wit_bindgen_wasmtime::export!({paths: ["../../wit/core/http.wit"]});
use http::{Http, HttpError, HttpMethod, HttpRequest, HttpRequestError, HttpResponse};

pub use http::add_to_linker;

pub struct HttpBindings {
    pub http_config: HttpRequestsConfig,
}

/// Implement the conversion between HttpMethod and
/// http::Method
impl From<HttpMethod> for reqwest::Method {
    fn from(value: HttpMethod) -> Self {
        match value {
            HttpMethod::Get => Method::GET,
            HttpMethod::Post => Method::POST,
            HttpMethod::Put => Method::PUT,
            HttpMethod::Patch => Method::PATCH,
            HttpMethod::Delete => Method::DELETE,
            HttpMethod::Options => Method::OPTIONS,
            HttpMethod::Head => Method::HEAD,
        }
    }
}

/// Map the reqwest error to a known http-error
/// HttpError comes from the HTTP bindings
impl From<reqwest::Error> for HttpError {
    fn from(value: reqwest::Error) -> Self {
        if value.is_timeout() {
            HttpError::Timeout
        } else if value.is_redirect() {
            HttpError::RedirectLoop
        } else if value.is_request() {
            HttpError::InvalidRequest
        } else if value.is_body() {
            HttpError::InvalidRequestBody
        } else if value.is_decode() {
            HttpError::InvalidResponseBody
        } else {
            HttpError::InternalError
        }
    }
}

type HttpHeaders = Vec<(String, String)>;

enum HttpRequestDispatchType {
    Local,
    Remote,
}

impl HttpBindings {
    fn is_request_allowed(&self, uri: &Uri, method: &Method) -> Result<(), HttpRequestError> {
        // Check that the host is allowed
        if uri.host().is_some()
            && !self
                .http_config
                .allowed_hosts
                .contains(&uri.host().unwrap().to_string())
        {
            return Err(HttpRequestError {
                error: HttpError::NotAllowed,
                message: format!(
                    "The host '{}' is not allowed for this worker. Please, update the worker configuration.",
                    uri.host().unwrap()
                ),
            });
        }

        // Check that the scheme is allowed
        if uri.scheme().is_some()
            && (!self.http_config.allow_http && uri.scheme_str().unwrap() == "http")
        {
            return Err(HttpRequestError {
                error: HttpError::NotAllowed,
                message:
                "The URI must use HTTPS. You can allow http requests in the worker configuration".to_string()
            });
        }

        // Check that the method is allowed
        if !self
            .http_config
            .allowed_methods
            .contains(&method.to_string())
        {
            return Err(HttpRequestError {
                error: HttpError::NotAllowed,
                message:
                format!("The method '{}' is not allowed for this worker. Please, update the configuration.", method.as_str())
            });
        }

        Ok(())
    }

    // Defines whether a request is meant to leave the WebAssembly
    // runtime and be made to the outer world.
    fn http_request_dispatch_type(
        &self,
        req: &HttpRequest<'_>,
    ) -> Result<HttpRequestDispatchType, HttpError> {
        let host = req
            .uri
            .parse::<Uri>()
            .map_err(|_| HttpError::InvalidRequest)?;
        if host.host() == Some("localhost") {
            Ok(HttpRequestDispatchType::Local)
        } else {
            Ok(HttpRequestDispatchType::Remote)
        }
    }

    // Dispatches an HTTP request made by a worker to another
    // worker. This request will not leave the WebAssembly runtime.
    fn dispatch_local_http_request(
        &self,
        req: &HttpRequest<'_>,
        headers: HttpHeaders,
    ) -> Result<HttpResponse, HttpRequestError> {
        Err(HttpRequestError {
            error: HttpError::InternalError,
            message: "Not implemented yet.".to_string(),
        })
    }

    // Dispatches the HTTP request made by a worker to an external
    // service. This request will leave the WebAssembly runtime.
    fn dispatch_remote_http_request(
        &self,
        req: &HttpRequest<'_>,
        headers: HttpHeaders,
    ) -> Result<HttpResponse, HttpRequestError> {
        let url = req.uri.to_string();
        let body = req.body.unwrap_or(&[]).to_vec();
        let uri = url.parse::<Uri>().map_err(|e| HttpRequestError {
            error: HttpError::InvalidRequest,
            message: e.to_string(),
        })?;
        let method: Method = req.method.into();

        // Check if the request is allowed
        self.is_request_allowed(&uri, &method)?;

        // Run the request in an async thread
        let thread_result = std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let client = reqwest::Client::new();

                    let mut builder = client.request(method, url);

                    for (key, value) in headers {
                        builder = builder.header(key, value);
                    }

                    builder = builder.body(body);

                    match builder.send().await {
                        Ok(res) => {
                            let mut headers = Vec::new();
                            let status = res.status().as_u16();

                            for (name, value) in res.headers().iter() {
                                headers
                                    .push((name.to_string(), value.to_str().unwrap().to_string()));
                            }

                            let body = res.bytes().await;

                            Ok(HttpResponse {
                                headers,
                                status,
                                body: Some(body.unwrap().to_vec()),
                            })
                        }
                        Err(e) => {
                            let message = e.to_string();

                            // Manage the different possible errors from Reqwest
                            Err(HttpRequestError {
                                error: e.into(),
                                message,
                            })
                        }
                    }
                })
        })
        .join();

        match thread_result {
            Ok(res) => match res {
                Ok(res) => Ok(res),
                Err(err) => Err(err),
            },
            Err(_) => Err(HttpRequestError {
                error: HttpError::InternalError,
                message: "There was an error processing the request on the host side.".to_string(),
            }),
        }
    }
}

impl Http for HttpBindings {
    fn send_http_request(
        &mut self,
        req: HttpRequest<'_>,
    ) -> Result<HttpResponse, HttpRequestError> {
        let url = req.uri.to_string();
        let body = req.body.unwrap_or(&[]).to_vec();
        let uri = url.parse::<Uri>().map_err(|e| HttpRequestError {
            error: HttpError::InvalidRequest,
            message: e.to_string(),
        })?;
        let method: Method = req.method.into();

        // Check if the request is allowed
        self.is_request_allowed(&uri, &method)?;

        // Create local variables from the request
        let mut headers = Vec::new();
        for (key, value) in &req.headers {
            headers.push((key.to_string(), value.to_string()));
        }

        let dispatch_type =
            self.http_request_dispatch_type(&req)
                .map_err(|_| HttpRequestError {
                    error: HttpError::InvalidRequest,
                    message: "error determining if the request is meant to be local or remote"
                        .to_string(),
                })?;
        match dispatch_type {
            HttpRequestDispatchType::Local => self.dispatch_local_http_request(&req, headers),
            HttpRequestDispatchType::Remote => self.dispatch_remote_http_request(&req, headers),
        }
    }
}
