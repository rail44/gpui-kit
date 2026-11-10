use crate::engine::quickjs::js_error;

use rquickjs::{
    ArrayBuffer, Ctx, FromJs, Function, IntoJs, Object, Promise, Result, TypedArray, Value,
    function::{Func, Opt},
};

use super::super::{host, scheduler};

use crate::net::http::{HttpRequest, HttpResponse, MAX_BODY_BYTES, authorize_request};

pub(super) fn install(ctx: &Ctx<'_>) -> Result<()> {
    ctx.globals().set("fetch", Func::from(fetch))
}

fn fetch<'js>(ctx: Ctx<'js>, url: String, options: Opt<FetchOptions>) -> Result<Promise<'js>> {
    let initial = reqwest::Url::parse(&url)
        .map_err(|error| js_error::type_error(&ctx, &format!("invalid fetch URL: {error}")))?;
    let options = options.0.unwrap_or_default();
    let capabilities = host::capabilities();
    authorize_request(&capabilities, &options.method, &initial)
        .map_err(|error| js_error::type_error(&ctx, &error))?;

    #[cfg(test)]
    let injected_client =
        crate::scope::current_runtime().and_then(|runtime| runtime.test_http_client());
    #[cfg(not(test))]
    let injected_client = None;

    scheduler::blocking(&ctx, "fetch(url, options)", move || {
        crate::net::http::request(
            capabilities,
            initial,
            HttpRequest {
                method: options.method,
                headers: options.headers,
                body: options.body,
            },
            injected_client,
        )
        .map(FetchResponse::from)
    })
}

/// The intentionally small, data-only request surface. It keeps HTTP traffic
/// capability-gated while allowing OAuth form exchanges and authenticated reads.
struct FetchOptions {
    method: reqwest::Method,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            method: reqwest::Method::GET,
            headers: reqwest::header::HeaderMap::new(),
            body: Vec::new(),
        }
    }
}

impl<'js> FromJs<'js> for FetchOptions {
    fn from_js(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Self> {
        if value.is_null() || value.is_undefined() {
            return Ok(Self::default());
        }
        let Some(object) = value.into_object() else {
            return Err(js_error::type_error(
                ctx,
                "fetch(url, options) expects an object with method, headers and/or body",
            ));
        };
        for key in object.keys::<String>() {
            let key = key?;
            if !matches!(key.as_str(), "method" | "headers" | "body") {
                return Err(js_error::type_error(
                    ctx,
                    &format!(
                        "unknown option `{key}` for fetch(url, options); expected method, headers or body"
                    ),
                ));
            }
        }

        let method = match object.get::<_, Option<String>>("method")? {
            None => reqwest::Method::GET,
            Some(method) => parse_method(ctx, &method)?,
        };
        let headers = parse_headers(ctx, object.get("headers")?)?;
        let body = parse_body(ctx, object.get("body")?)?;
        Ok(Self {
            method,
            headers,
            body,
        })
    }
}

/// Any method the capability policy is willing to grant.
///
/// This is a token check rather than a list. What may be sent, to which host,
/// on which path, is `Capabilities::may_request`'s decision and it already
/// takes the method -- a second list here would be a second policy to keep in
/// step with the first, and the way that goes wrong is a grant that cannot be
/// exercised. So a method that is a method is parsed, and refusing it stays
/// where refusing belongs.
///
/// What is refused here is a string that is not an HTTP method at all: an
/// empty field, a space, a quote. `Method::from_bytes` is exactly that judge.
/// Known methods are upper-cased on the way through, as `fetch` does.
fn parse_method(ctx: &Ctx<'_>, method: &str) -> Result<reqwest::Method> {
    reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes()).map_err(|_| {
        js_error::type_error(
            ctx,
            &format!("fetch(url, options).method `{method}` is not an HTTP method"),
        )
    })
}

fn parse_headers<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<reqwest::header::HeaderMap> {
    if value.is_null() || value.is_undefined() {
        return Ok(reqwest::header::HeaderMap::new());
    }
    let Some(object) = value.into_object() else {
        return Err(js_error::type_error(
            ctx,
            "fetch(url, options).headers expects a plain object of string values",
        ));
    };

    let mut headers = reqwest::header::HeaderMap::new();
    for entry in object.props::<String, Value>() {
        let (name, value) = entry?;
        if prohibited_header(&name) {
            return Err(js_error::type_error(
                ctx,
                &format!("fetch(url, options).headers may not set `{name}`"),
            ));
        }
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            js_error::type_error(
                ctx,
                "fetch(url, options).headers has an invalid header name",
            )
        })?;
        let value = String::from_js(ctx, value).map_err(|_| {
            js_error::type_error(
                ctx,
                "fetch(url, options).headers expects string header values",
            )
        })?;
        let value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            js_error::type_error(
                ctx,
                "fetch(url, options).headers has an invalid header value",
            )
        })?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn prohibited_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "connection"
            | "expect"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn parse_body<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Vec<u8>> {
    if value.is_null() || value.is_undefined() {
        return Ok(Vec::new());
    }
    if value.is_string() {
        return limited_body(ctx, String::from_js(ctx, value)?.into_bytes());
    }
    let bytes = TypedArray::<u8>::from_js(ctx, value).map_err(|_| {
        js_error::type_error(
            ctx,
            "fetch(url, options).body expects a string or Uint8Array",
        )
    })?;
    // SAFETY: finish copying before error creation or other JavaScript can run.
    let bytes = unsafe { bytes.as_bytes().map(<[u8]>::to_vec) }.ok_or_else(|| {
        js_error::type_error(
            ctx,
            "fetch(url, options).body received a detached Uint8Array",
        )
    })?;
    limited_body(ctx, bytes)
}

fn limited_body<'js>(ctx: &Ctx<'js>, body: Vec<u8>) -> Result<Vec<u8>> {
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(js_error::range_error(
            ctx,
            &format!("fetch request body exceeded the {MAX_BODY_BYTES} byte limit"),
        ));
    }
    Ok(body)
}

impl From<HttpResponse> for FetchResponse {
    fn from(response: HttpResponse) -> Self {
        Self {
            status: response.status,
            url: response.url,
            body: response.body,
        }
    }
}

struct FetchResponse {
    status: u16,
    url: String,
    body: Vec<u8>,
}

impl<'js> IntoJs<'js> for FetchResponse {
    fn into_js(self, ctx: &Ctx<'js>) -> Result<Value<'js>> {
        let response = Object::new(ctx.clone())?;
        response.set("status", self.status)?;
        response.set("ok", (200..300).contains(&self.status))?;
        response.set("url", self.url)?;
        let body = std::rc::Rc::new(self.body);
        let text = String::from_utf8_lossy(&body).into_owned();
        let text_method = text.clone();
        let buffer_body = body.clone();
        response.set(
            "arrayBuffer",
            Func::from(move |ctx: Ctx<'js>| -> Result<Promise<'js>> {
                let (promise, resolve, _) = Promise::new(&ctx)?;
                let buffer = ArrayBuffer::new_copy(ctx.clone(), buffer_body.as_slice())?;
                resolve.call::<_, ()>((buffer,))?;
                Ok(promise)
            }),
        )?;
        response.set(
            "bytes",
            Func::from(move |ctx: Ctx<'js>| -> Result<Promise<'js>> {
                let (promise, resolve, _) = Promise::new(&ctx)?;
                let bytes = TypedArray::<u8>::new_copy(ctx.clone(), body.as_slice())?;
                resolve.call::<_, ()>((bytes,))?;
                Ok(promise)
            }),
        )?;
        response.set(
            "text",
            Func::from(move |ctx: Ctx<'js>| -> Result<Promise<'js>> {
                let (promise, resolve, _) = Promise::new(&ctx)?;
                resolve.call::<_, ()>((text_method.clone(),))?;
                Ok(promise)
            }),
        )?;
        response.set(
            "json",
            Func::from(move |ctx: Ctx<'js>| -> Result<Promise<'js>> {
                let (promise, resolve, reject) = Promise::new(&ctx)?;
                let json: Object = ctx.globals().get("JSON")?;
                let parse: Function = json.get("parse")?;
                match parse.call::<_, Value>((text.clone(),)) {
                    Ok(value) => resolve.call::<_, ()>((value,))?,
                    Err(rquickjs::Error::Exception) => {
                        reject.call::<_, ()>((ctx.catch(),))?;
                    }
                    Err(error) => return Err(error),
                }
                Ok(promise)
            }),
        )?;
        Ok(response.into_value())
    }
}
