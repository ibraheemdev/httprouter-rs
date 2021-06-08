//! [`Router`](crate::Router) is a lightweight high performance HTTP request router.
//!
//! This router supports variables in the routing pattern and matches against
//! the request method. It also scales better.
//!
//! The router is optimized for high performance and a small memory footprint.
//! It scales well even with very long paths and a large number of routes.
//! A compressing dynamic trie (radix tree) structure is used for efficient matching.
//!
//! With the `hyper-server` feature enabled, the `Router` can be used as a router for a hyper server:
//!
//! ```rust,no_run
//! use httprouter::{Router, Params, handler_fn};
//! use std::convert::Infallible;
//! use hyper::{Request, Response, Body, Error};
//!
//! async fn index(_: Request<Body>) -> Result<Response<Body>, Error> {
//!     Ok(Response::new("Hello, World!".into()))
//! }
//!
//! async fn hello(req: Request<Body>) -> Result<Response<Body>, Error> {
//!     let params = req.extensions().get::<Params>().unwrap();
//!     Ok(Response::new(format!("Hello, {}", params.get("user").unwrap()).into()))
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = Router::default()
//!         .get("/", handler_fn(index))
//!         .get("/hello/:user", handler_fn(hello));
//!
//!     hyper::Server::bind(&([127, 0, 0, 1], 3000).into())
//!         .serve(router.into_service())
//!         .await;
//! }
//!```
//!
//! The registered path, against which the router matches incoming requests, can
//! contain two types of parameters:
//! ```ignore
//!  Syntax    Type
//!  :name     named parameter
//!  *name     catch-all parameter
//! ```
//!
//! Named parameters are dynamic path segments. They match anything until the
//! next '/' or the path end:
//! ```ignore
//!  Path: /blog/:category/:post
//! ```
//!
//!  Requests:
//! ```ignore
//!   /blog/rust/request-routers            match: category="rust", post="request-routers"
//!   /blog/rust/request-routers/           no match, but the router would redirect
//!   /blog/rust/                           no match
//!   /blog/rust/request-routers/comments   no match
//! ```
//!
//! Catch-all parameters match anything until the path end, including the
//! directory index (the '/' before the catch-all). Since they match anything
//! until the end, catch-all parameters must always be the final path element.
//!  Path: /files/*filepath
//!
//!  Requests:
//! ```ignore
//!   /files/                             match: filepath="/"
//!   /files/LICENSE                      match: filepath="/LICENSE"
//!   /files/templates/article.html       match: filepath="/templates/article.html"
//!   /files                              no match, but the router would redirect
//! ```
//! The value of parameters is saved as a `Vec` of the `Param` struct, consisting
//! each of a key and a value.
//! ```ignore
//! # use httprouter::tree::Params;
//! # let params = Params::default();
//!
//! let user = params.get("user") // defined by :user or *user
//!
//! // alternatively, you can iterate through every matched parameter
//! for (k, v) in params.iter() {
//!    println!("{}: {}", k, v")
//! }
//! ```
use crate::path::clean;

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::{future, ready};
use hyper::service::Service;
use hyper::{header, Body, Method, Request, Response, StatusCode};
use matchit::Node;

#[derive(Default)]
pub struct Params {
    vec: Vec<(String, String)>,
}

impl Params {
    /// Returns the value of the first parameter registered matched for the given key.
    pub fn get(&self, key: impl AsRef<str>) -> Option<&str> {
        self.vec
            .iter()
            .find(|(k, _)| k == key.as_ref())
            .map(|(_, v)| v.as_str())
    }

    /// Returns an iterator over the parameters in the list.
    pub fn iter(&self) -> std::slice::Iter<'_, (String, String)> {
        self.vec.iter()
    }

    /// Returns a mutable iterator over the parameters in the list.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, (String, String)> {
        self.vec.iter_mut()
    }

    /// Returns an owned iterator over the parameters in the list.
    pub fn into_iter(self) -> std::vec::IntoIter<(String, String)> {
        self.vec.into_iter()
    }
}

pub trait HandlerService<F, E>:
    Service<Request<Body>, Response = Response<Body>, Error = E, Future = F>
    + Send
    + Sync
    + Clone
    + 'static
{
}

impl<S, F, E> HandlerService<F, E> for S where
    S: Service<Request<Body>, Response = Response<Body>, Error = E, Future = F>
        + Send
        + Sync
        + Clone
        + 'static
{
}

pub trait HandlerFuture<E>:
    Future<Output = Result<Response<Body>, E>> + Send + Sync + 'static
{
}

impl<F, E> HandlerFuture<E> for F where
    F: Future<Output = Result<Response<Body>, E>> + Send + Sync + 'static
{
}

pub trait HandlerError: StdError + Send + Sync + 'static {}

impl<E> HandlerError for E where E: StdError + Send + Sync + 'static {}

#[derive(Clone)]
struct HandlerServiceImpl<S> {
    service: S,
}

impl<S> HandlerServiceImpl<S> {
    fn new(service: S) -> Self {
        Self { service }
    }
}

impl<S, R> Service<R> for HandlerServiceImpl<S>
where
    S: Service<R>,
    S::Future: Send + Sync + 'static,
    S::Error: HandlerError,
{
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, BoxError>> + Send + Sync>>;
    type Error = BoxError;
    type Response = S::Response;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: R) -> Self::Future {
        use futures_util::future::TryFutureExt;
        Box::pin(self.service.call(req).map_err(|e| BoxError(Box::new(e))))
    }
}

pub fn handler_fn<F, O, E>(f: F) -> HandlerFnService<F>
where
    F: FnMut(Request<Body>) -> O + Send + Sync + Clone + 'static,
    O: HandlerFuture<E>,
    E: HandlerError,
{
    fn assert_handler<H, O, E>(h: H) -> H
    where
        H: HandlerService<O, E>,
        O: HandlerFuture<E>,
        E: HandlerError,
    {
        h
    }

    assert_handler(HandlerFnService { f })
}

#[doc(hidden)]
#[derive(Clone)]
pub struct HandlerFnService<F> {
    f: F,
}

impl<F, O, E> Service<Request<Body>> for HandlerFnService<F>
where
    F: FnMut(Request<Body>) -> O,
    O: Future<Output = Result<Response<Body>, E>> + Send + Sync + 'static,
    E: HandlerError,
{
    type Response = Response<Body>;
    type Error = E;
    type Future = O;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        (self.f)(req)
    }
}

trait StoredService:
    Service<
        Request<Body>,
        Error = BoxError,
        Response = Response<Body>,
        Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send + Sync>>,
    > + Send
    + Sync
    + 'static
{
    fn box_clone(&self) -> Box<dyn StoredService>;
}

impl<S> StoredService for S
where
    S: Service<
            Request<Body>,
            Error = BoxError,
            Response = Response<Body>,
            Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send + Sync>>,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    fn box_clone(&self) -> Box<dyn StoredService> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn StoredService> {
    fn clone(&self) -> Self {
        self.box_clone()
    }
}

pub struct Router {
    trees: HashMap<Method, Node<Box<dyn StoredService>>>,
    redirect_trailing_slash: bool,
    redirect_fixed_path: bool,
    handle_method_not_allowed: bool,
    handle_options: bool,
    global_options: Option<Box<dyn StoredService>>,
    not_found: Option<Box<dyn StoredService>>,
    method_not_allowed: Option<Box<dyn StoredService>>,
}

impl Router {
    /// Register a handler for the given path and method.
    /// ```rust
    /// use httprouter::{Router, handler_fn};
    /// use hyper::{Response, Body, Method};
    /// use std::convert::Infallible;
    ///
    /// let router = Router::default()
    ///     .handle("/teapot", Method::GET, handler_fn(|_| async {
    ///         Ok::<_, Infallible>(Response::new(Body::from("I am a teapot!")))
    ///     }));
    /// ```
    pub fn handle<H, F, E>(mut self, path: impl Into<String>, method: Method, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        let path = path.into();
        if !path.starts_with('/') {
            panic!("expect path beginning with '/', found: '{}'", path);
        }

        self.trees
            .entry(method)
            .or_insert_with(Node::default)
            .insert(path, Box::new(HandlerServiceImpl::new(handler)))
            .unwrap();

        self
    }

    /// TODO
    pub fn serve_files() {
        unimplemented!()
    }

    /// Register a handler for `GET` requests
    pub fn get<H, F, E>(self, path: impl Into<String>, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.handle(path, Method::GET, handler)
    }

    /// Register a handler for `HEAD` requests
    pub fn head<H, F, E>(self, path: impl Into<String>, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.handle(path, Method::HEAD, handler)
    }

    /// Register a handler for `OPTIONS` requests
    pub fn options<H, F, E>(self, path: impl Into<String>, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.handle(path, Method::OPTIONS, handler)
    }

    /// Register a handler for `POST` requests
    pub fn post<H, F, E>(self, path: impl Into<String>, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.handle(path, Method::POST, handler)
    }

    /// Register a handler for `PUT` requests
    pub fn put<H, F, E>(self, path: impl Into<String>, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.handle(path, Method::PUT, handler)
    }

    /// Register a handler for `PATCH` requests
    pub fn patch<H, F, E>(self, path: impl Into<String>, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.handle(path, Method::PATCH, handler)
    }

    /// Register a handler for `DELETE` requests
    pub fn delete<H, F, E>(self, path: impl Into<String>, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.handle(path, Method::DELETE, handler)
    }

    /// Enables automatic redirection if the current route can't be matched but a
    /// handler for the path with (without) the trailing slash exists.
    /// For example if `/foo/` is requested but a route only exists for `/foo`, the
    /// client is redirected to `/foo` with HTTP status code 301 for `GET` requests
    /// and 307 for all other request methods.
    pub fn redirect_trailing_slash(mut self) -> Self {
        self.redirect_trailing_slash = true;
        self
    }

    /// If enabled, the router tries to fix the current request path, if no
    /// handle is registered for it.
    /// First superfluous path elements like `../` or `//` are removed.
    /// Afterwards the router does a case-insensitive lookup of the cleaned path.
    /// If a handle can be found for this route, the router makes a redirection
    /// to the corrected path with status code 301 for `GET` requests and 307 for
    /// all other request methods.
    /// For example `/FOO` and `/..//Foo` could be redirected to `/foo`.
    /// `redirect_trailing_slash` is independent of this option.
    pub fn redirect_fixed_path(mut self) -> Self {
        self.redirect_fixed_path = true;
        self
    }

    /// If enabled, the router checks if another method is allowed for the
    /// current route, if the current request can not be routed.
    /// If this is the case, the request is answered with `MethodNotAllowed`
    /// and HTTP status code 405.
    /// If no other Method is allowed, the request is delegated to the `NotFound`
    /// handler.
    pub fn handle_method_not_allowed(mut self) -> Self {
        self.handle_method_not_allowed = true;
        self
    }

    /// If enabled, the router automatically replies to `OPTIONS` requests.
    /// Custom `OPTIONS` handlers take priority over automatic replies.
    pub fn handle_options(mut self) -> Self {
        self.handle_options = true;
        self
    }

    /// An optional handler that is called on automatic `OPTIONS` requests.
    /// The handler is only called if `handle_options` is true and no `OPTIONS`
    /// handler for the specific path was set.
    /// The `Allowed` header is set before calling the handler.
    pub fn global_options<H, F, E>(mut self, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.global_options = Some(Box::new(HandlerServiceImpl::new(handler)));
        self
    }

    /// Configurable handler which is called when no matching route is
    /// found.
    pub fn not_found<H, F, E>(mut self, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.not_found = Some(Box::new(HandlerServiceImpl::new(handler)));
        self
    }

    /// A configurable handler which is called when a request
    /// cannot be routed and `handle_method_not_allowed` is true.
    /// The `Allow` header with allowed request methods is set before the handler
    /// is called.
    pub fn method_not_allowed<H, F, E>(mut self, handler: H) -> Self
    where
        H: HandlerService<F, E>,
        F: HandlerFuture<E>,
        E: HandlerError,
    {
        self.method_not_allowed = Some(Box::new(HandlerServiceImpl::new(handler)));
        self
    }

    /// Returns a list of the allowed methods for a specific path
    /// ```rust
    /// use httprouter::{Router, handler_fn};
    /// use hyper::{Response, Body, Method};
    /// use std::convert::Infallible;
    ///
    /// let router = Router::default()
    ///     .get("/home", handler_fn(|_| async {
    ///         Ok::<_, Infallible>(Response::new(Body::from("Welcome!")))
    ///     }))
    ///     .post("/home", handler_fn(|_| async {
    ///         Ok::<_, Infallible>(Response::new(Body::from("{{ message: \"Welcome!\" }}")))
    ///     }));
    ///
    /// let allowed = router.allowed("/home");
    /// assert!(allowed.contains(&"GET"));
    /// assert!(allowed.contains(&"POST"));
    /// assert!(allowed.contains(&"OPTIONS"));
    /// # assert_eq!(allowed.len(), 3);
    /// ```
    pub fn allowed(&self, path: impl Into<String>) -> Vec<&str> {
        let path = path.into();
        let mut allowed = match path.as_ref() {
            "*" => {
                let mut allowed = Vec::with_capacity(self.trees.len());
                for method in self
                    .trees
                    .keys()
                    .filter(|&method| method != Method::OPTIONS)
                {
                    allowed.push(method.as_ref());
                }
                allowed
            }
            _ => self
                .trees
                .keys()
                .filter(|&method| method != Method::OPTIONS)
                .filter(|&method| {
                    self.trees
                        .get(method)
                        .map(|node| node.at(&path).is_ok())
                        .unwrap_or(false)
                })
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
        };

        if !allowed.is_empty() {
            allowed.push(Method::OPTIONS.as_ref())
        }

        allowed
    }
}

impl Default for Router {
    fn default() -> Self {
        Self {
            trees: HashMap::new(),
            redirect_trailing_slash: true,
            redirect_fixed_path: true,
            handle_method_not_allowed: true,
            handle_options: true,
            global_options: None,
            method_not_allowed: None,
            not_found: Some(Box::new(HandlerServiceImpl::new(handler_fn(|_| async {
                Ok::<_, hyper::Error>(Response::builder().status(400).body(Body::empty()).unwrap())
            })))),
        }
    }
}

#[doc(hidden)]
pub struct MakeRouterService(RouterService);

impl<T> Service<T> for MakeRouterService {
    type Response = RouterService;
    type Error = hyper::Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: T) -> Self::Future {
        let service = self.0.clone();
        future::ok(service)
    }
}

#[doc(hidden)]
#[derive(Clone)]
pub struct RouterService(Arc<Router>);

impl RouterService {
    fn new(router: Router) -> Self {
        RouterService(Arc::new(router))
    }
}

impl Service<Request<Body>> for RouterService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = ResponseFut;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        self.0.clone().serve(req)
    }
}

impl Router {
    /// Converts the `Router` into a `Service` which you can serve directly with `Hyper`.
    /// If you have an existing `Service` that you want to incorporate a `Router` into, see
    /// [`Router::serve`](crate::Router::serve).
    /// ```rust,no_run
    /// # use httprouter::Router;
    /// # use std::convert::Infallible;
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// // Our router...
    /// let router = Router::default();
    ///
    /// // Convert it into a service...
    /// let service = router.into_service();
    ///
    /// // Serve with hyper
    /// hyper::Server::bind(&([127, 0, 0, 1], 3030).into())
    ///     .serve(service)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_service(self) -> MakeRouterService {
        MakeRouterService(RouterService::new(self))
    }

    /// An asynchronous function from a `Request` to a `Response`. You will generally not need to use
    /// this function directly, and instead use
    /// [`Router::into_service`](crate::Router::into_service). However, it may be useful when
    /// incorporating the router into a larger service.
    /// ```rust,no_run
    /// # use httprouter::Router;
    /// # use hyper::service::{make_service_fn, service_fn};
    /// # use hyper::{Request, Body, Server};
    /// # use std::convert::Infallible;
    /// # use std::sync::Arc;
    ///
    /// # async fn run() {
    /// let router = Arc::new(Router::default());
    ///
    /// let make_svc = make_service_fn(move |_| {
    ///     let router = router.clone();
    ///     async move {
    ///         Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
    ///             let router = router.clone();
    ///             async move { router.serve(req).await }
    ///         }))
    ///     }
    /// });
    ///
    /// let server = Server::bind(&([127, 0, 0, 1], 3000).into())
    ///     .serve(make_svc)
    ///     .await;
    /// # }
    /// ```
    pub fn serve(&self, mut req: Request<Body>) -> ResponseFut {
        let root = self.trees.get(req.method());
        let path = req.uri().path();
        if let Some(root) = root {
            match root.at(path) {
                Ok(lookup) => {
                    let mut value = lookup.value.clone();
                    let vec = lookup
                        .params
                        .iter()
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                        .collect();
                    req.extensions_mut().insert(Params { vec });
                    return ResponseFutKind::Boxed(value.call(req)).into();
                }
                Err(err) => {
                    if req.method() != Method::CONNECT && path != "/" {
                        let code = match *req.method() {
                            // Moved Permanently, request with GET method
                            Method::GET => StatusCode::MOVED_PERMANENTLY,
                            // Permanent Redirect, request with same method
                            _ => StatusCode::PERMANENT_REDIRECT,
                        };

                        if err.tsr() && self.redirect_trailing_slash {
                            let path = if path.len() > 1 && path.ends_with('/') {
                                path[..path.len() - 1].to_owned()
                            } else {
                                [path, "/"].join("")
                            };

                            return ResponseFutKind::Redirect(path, code).into();
                        }

                        if self.redirect_fixed_path {
                            if let Some(fixed_path) =
                                root.path_ignore_case(clean(path), self.redirect_trailing_slash)
                            {
                                return ResponseFutKind::Redirect(fixed_path, code).into();
                            }
                        }
                    }
                }
            }
        }

        if req.method() == Method::OPTIONS && self.handle_options {
            let allow = self.allowed(path);

            if !allow.is_empty() {
                return match self.global_options {
                    Some(ref handler) => ResponseFutKind::Boxed(handler.clone().call(req)).into(),
                    None => ResponseFutKind::Options(allow.join(", ")).into(),
                };
            }
        } else if self.handle_method_not_allowed {
            let allow = self.allowed(path);

            if !allow.is_empty() {
                return match self.method_not_allowed {
                    Some(ref handler) => ResponseFutKind::Boxed(handler.clone().call(req)).into(),
                    None => ResponseFutKind::MethodNotAllowed(allow.join(", ")).into(),
                };
            }
        }

        match self.not_found {
            Some(ref handler) => ResponseFutKind::Boxed(handler.clone().call(req)).into(),
            None => ResponseFutKind::NotFound.into(),
        }
    }
}

pub struct ResponseFut {
    kind: ResponseFutKind,
}

impl From<ResponseFutKind> for ResponseFut {
    fn from(kind: ResponseFutKind) -> Self {
        Self { kind }
    }
}

enum ResponseFutKind {
    Boxed(Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send + Sync>>),
    Redirect(String, StatusCode),
    MethodNotAllowed(String),
    Options(String),
    NotFound,
}

impl Future for ResponseFut {
    type Output = Result<Response<Body>, BoxError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ready = match self.kind {
            ResponseFutKind::Boxed(ref mut fut) => ready!(fut.as_mut().poll(cx)),
            ResponseFutKind::Redirect(ref path, code) => Ok(Response::builder()
                .header(header::LOCATION, path.as_str())
                .status(code)
                .body(Body::empty())
                .unwrap()),
            ResponseFutKind::NotFound => Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap()),
            ResponseFutKind::Options(ref allowed) => Ok(Response::builder()
                .header(header::ALLOW, allowed)
                .body(Body::empty())
                .unwrap()),
            ResponseFutKind::MethodNotAllowed(ref allowed) => Ok(Response::builder()
                .header(header::ALLOW, allowed)
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(Body::empty())
                .unwrap()),
        };

        Poll::Ready(ready)
    }
}

pub struct BoxError(Box<dyn StdError + Send + Sync>);

impl fmt::Display for BoxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for BoxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl StdError for BoxError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.0)
    }
}
