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
//! use httprouter::{Router, Params};
//! use std::convert::Infallible;
//! use hyper::{Request, Response, Body, Error};
//!
//! async fn index(_: Request<Body>) -> Result<Response<Body>, Error> {
//!     Ok(Response::new("Hello, World!".into()))
//! }
//!
//! async fn hello(req: Request<Body>) -> Result<Response<Body>, Error> {
//!     let params = req.extensions().get::<Params>().unwrap();
//!     Ok(Response::new(format!("Hello, {}", params.by_name("user").unwrap()).into()))
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut router: Router = Router::default();
//!     router.get("/", index);
//!     router.get("/hello/:user", hello);
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
//!
//! There are two ways to retrieve the value of a parameter:
//!  1) by the name of the parameter
//! ```ignore
//!  # use httprouter::tree::Params;
//!  # let params = Params::default();

//!  let user = params.by_name("user") // defined by :user or *user
//! ```
//!  2) by the index of the parameter. This way you can also get the name (key)
//! ```rust,no_run
//!  # use httprouter::Params;
//!  # let params = Params::default();
//!  let third_key = &params[2].key;   // the name of the 3rd parameter
//!  let third_value = &params[2].value; // the value of the 3rd parameter
//! ```
use crate::path::clean;
use hyper::service::Service;
use hyper::{header, Body, Method, Request, Response, StatusCode};
use matchit::{Match, Node};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::str;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Router dispatches requests to different handlers via configurable routes.
pub struct Router {
  trees: HashMap<Method, Node<Box<dyn Handler>>>,

  /// Enables automatic redirection if the current route can't be matched but a
  /// handler for the path with (without) the trailing slash exists.
  /// For example if `/foo/` is requested but a route only exists for `/foo`, the
  /// client is redirected to `/foo` with HTTP status code 301 for `GET` requests
  /// and 307 for all other request methods.
  pub redirect_trailing_slash: bool,

  /// If enabled, the router tries to fix the current request path, if no
  /// handle is registered for it.
  /// First superfluous path elements like `../` or `//` are removed.
  /// Afterwards the router does a case-insensitive lookup of the cleaned path.
  /// If a handle can be found for this route, the router makes a redirection
  /// to the corrected path with status code 301 for `GET` requests and 307 for
  /// all other request methods.
  /// For example `/FOO` and `/..//Foo` could be redirected to `/foo`.
  /// `redirect_trailing_slash` is independent of this option.
  pub redirect_fixed_path: bool,

  /// If enabled, the router checks if another method is allowed for the
  /// current route, if the current request can not be routed.
  /// If this is the case, the request is answered with `MethodNotAllowed`
  /// and HTTP status code 405.
  /// If no other Method is allowed, the request is delegated to the `NotFound`
  /// handler.
  pub handle_method_not_allowed: bool,

  /// If enabled, the router automatically replies to `OPTIONS` requests.
  /// Custom `OPTIONS` handlers take priority over automatic replies.
  pub handle_options: bool,

  /// An optional handler that is called on automatic `OPTIONS` requests.
  /// The handler is only called if `handle_options` is true and no `OPTIONS`
  /// handler for the specific path was set.
  /// The `Allowed` header is set before calling the handler.
  pub global_options: Option<Box<dyn Handler>>,

  /// Configurable handler which is called when no matching route is
  /// found.
  pub not_found: Option<Box<dyn Handler>>,

  /// A configurable handler which is called when a request
  /// cannot be routed and `handle_method_not_allowed` is true.
  /// The `Allow` header with allowed request methods is set before the handler
  /// is called.
  pub method_not_allowed: Option<Box<dyn Handler>>,
}

impl Router {
  /// Insert a value into the router for a specific path indexed by a key.
  /// ```rust
  /// use httprouter::Router;
  /// use hyper::{Response, Body, Method};
  ///
  /// let mut router: Router = Router::default();
  /// router.handle("/teapot", Method::GET, |_| {
  ///     async { Ok(Response::new(Body::from("I am a teapot!"))) }
  /// });
  /// ```
  pub fn handle(&mut self, path: &str, method: Method, handler: impl Handler + 'static + 'static) {
    if !path.starts_with('/') {
      panic!("path must begin with '/' in path '{}'", path);
    }

    self
      .trees
      .entry(method)
      .or_insert_with(Node::default)
      .insert(path, Box::new(handler));
  }

  /// Lookup allows the manual lookup of handler for a specific method and path.
  /// If the handler is not found, it returns a `Err(bool)` indicating whether a redirection should be performed to the same path with a trailing slash
  /// ```rust
  /// use httprouter::Router;
  /// use hyper::{Response, Body, Method};
  ///
  /// let mut router = Router::default();
  /// router.get("/home", |_| {
  ///     async { Ok(Response::new(Body::from("Welcome!"))) }
  /// });
  ///
  /// let res = router.lookup(&Method::GET, "/home").unwrap();
  /// assert!(res.params.is_empty());
  /// ```
  pub fn lookup(&self, method: &Method, path: &str) -> Result<Match<Box<dyn Handler>>, bool> {
    self
      .trees
      .get(method)
      .map_or(Err(false), |n| n.match_path(path))
  }

  /// TODO
  pub fn serve_files() {
    unimplemented!()
  }

  /// Register a handler for `GET` requests
  pub fn get(&mut self, path: &str, handler: impl Handler + 'static) {
    self.handle(path, Method::GET, handler);
  }

  /// Register a handler for `HEAD` requests
  pub fn head(&mut self, path: &str, handler: impl Handler + 'static) {
    self.handle(path, Method::HEAD, handler);
  }

  /// Register a handler for `OPTIONS` requests
  pub fn options(&mut self, path: &str, handler: impl Handler + 'static) {
    self.handle(path, Method::OPTIONS, handler);
  }

  /// Register a handler for `POST` requests
  pub fn post(&mut self, path: &str, handler: impl Handler + 'static) {
    self.handle(path, Method::POST, handler);
  }

  /// Register a handler for `PUT` requests
  pub fn put(&mut self, path: &str, handler: impl Handler + 'static) {
    self.handle(path, Method::PUT, handler);
  }

  /// Register a handler for `PATCH` requests
  pub fn patch(&mut self, path: &str, handler: impl Handler + 'static) {
    self.handle(path, Method::PATCH, handler);
  }

  /// Register a handler for `DELETE` requests
  pub fn delete(&mut self, path: &str, handler: impl Handler + 'static) {
    self.handle(path, Method::DELETE, handler);
  }

  /// Returns a list of the allowed methods for a specific path
  /// ```rust
  /// use httprouter::Router;
  /// use hyper::{Response, Body, Method};
  ///
  /// let mut router = Router::default();
  /// router.get("/home", |_| {
  ///     async { Ok(Response::new(Body::from("Welcome!"))) }
  /// });
  ///
  /// let allowed = router.allowed("/home");
  /// assert!(allowed.contains(&"GET".to_string()));
  /// ```
  pub fn allowed(&self, path: &str) -> Vec<String> {
    let mut allowed: Vec<String> = Vec::new();
    match path {
      "*" => {
        for method in self.trees.keys() {
          if method != Method::OPTIONS {
            allowed.push(method.to_string());
          }
        }
      }
      _ => {
        for method in self.trees.keys() {
          if method == Method::OPTIONS {
            continue;
          }

          if let Some(tree) = self.trees.get(method) {
            let handler = tree.match_path(path);

            if handler.is_ok() {
              allowed.push(method.to_string());
            }
          };
        }
      }
    };

    if !allowed.is_empty() {
      allowed.push(Method::OPTIONS.to_string())
    }

    allowed
  }
}

/// The default httprouter configuration
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
      not_found: Some(Box::new(|_| async {
        Ok(
          Response::builder()
            .status(400)
            .body(Body::from("404: Not Found"))
            .unwrap(),
        )
      })),
    }
  }
}

/// Represents a HTTP handler function.
/// This trait is implemented for asynchronous functions that take a `Request` and return a
/// `Result<Response<Body>, hyper::Error>`
/// ```rust
/// # use httprouter::Handler;
/// # use hyper::{Request, Response, Body};
/// async fn hello(_: Request<Body>) -> Result<Response<Body>, hyper::Error> {
///     Ok(Response::new(Body::empty()))
/// }
///
/// let handler: Box<dyn Handler> = Box::new(hello);
/// ```
pub trait Handler: Send + Sync {
  fn handle(
    &self,
    req: Request<Body>,
  ) -> Pin<Box<dyn Future<Output = hyper::Result<Response<Body>>> + Send + Sync>>;
}

impl<F, R> Handler for F
where
  F: Fn(Request<Body>) -> R + Send + Sync,
  R: Future<Output = Result<Response<Body>, hyper::Error>> + Send + Sync + 'static,
{
  fn handle(
    &self,
    req: Request<Body>,
  ) -> Pin<Box<dyn Future<Output = hyper::Result<Response<Body>>> + Send + Sync>> {
    Box::pin(self(req))
  }
}

#[doc(hidden)]
pub struct MakeRouterService(RouterService);

impl<T> Service<T> for MakeRouterService {
  type Response = RouterService;
  type Error = hyper::Error;
  type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

  fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
    Poll::Ready(Ok(()))
  }

  fn call(&mut self, _: T) -> Self::Future {
    let service = self.0.clone();
    let fut = async move { Ok(service) };
    Box::pin(fut)
  }
}

#[derive(Clone)]
#[doc(hidden)]
pub struct RouterService(Arc<Router>);

impl RouterService {
  fn new(router: Router) -> Self {
    RouterService(Arc::new(router))
  }
}

impl Service<Request<Body>> for RouterService {
  type Response = Response<Body>;
  type Error = hyper::Error;
  type Future = Pin<Box<dyn Future<Output = hyper::Result<Response<Body>>> + Send + Sync>>;

  fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    Poll::Ready(Ok(()))
  }

  fn call(&mut self, req: Request<Body>) -> Self::Future {
    self.0.serve(req)
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
  pub fn into_service<'a>(self) -> MakeRouterService {
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
  /// let mut router: Router = Router::default();
  ///
  /// let router = Arc::new(router);
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
  pub fn serve(
    &self,
    mut req: Request<Body>,
  ) -> Pin<Box<dyn Future<Output = hyper::Result<Response<Body>>> + Send + Sync>> {
    let root = self.trees.get(req.method());
    let path = req.uri().path();
    if let Some(root) = root {
      match root.match_path(path) {
        Ok(lookup) => {
          req.extensions_mut().insert(lookup.params);
          return lookup.value.handle(req);
        }
        Err(tsr) => {
          if req.method() != Method::CONNECT && path != "/" {
            let code = match *req.method() {
              // Moved Permanently, request with GET method
              Method::GET => StatusCode::MOVED_PERMANENTLY,
              // Permanent Redirect, request with same method
              _ => StatusCode::PERMANENT_REDIRECT,
            };

            if tsr && self.redirect_trailing_slash {
              let path = if path.len() > 1 && path.ends_with('/') {
                path[..path.len() - 1].to_string()
              } else {
                path.to_string() + "/"
              };

              return Box::pin(async move {
                Ok(
                  Response::builder()
                    .header(header::LOCATION, path.as_str())
                    .status(code)
                    .body(Body::empty())
                    .unwrap(),
                )
              });
            };

            if self.redirect_fixed_path {
              if let Some(fixed_path) =
                root.find_case_insensitive_path(&clean(path), self.redirect_trailing_slash)
              {
                return Box::pin(async move {
                  Ok(
                    Response::builder()
                      .header(header::LOCATION, fixed_path.as_str())
                      .status(code)
                      .body(Body::empty())
                      .unwrap(),
                  )
                });
              }
            };
          };
        }
      }
    };

    if req.method() == Method::OPTIONS && self.handle_options {
      let allow = self.allowed(path).join(", ");
      if allow != "" {
        match &self.global_options {
          Some(handler) => return handler.handle(req),
          None => {
            return Box::pin(async {
              Ok(
                Response::builder()
                  .header(header::ALLOW, allow)
                  .body(Body::empty())
                  .unwrap(),
              )
            });
          }
        };
      }
    } else if self.handle_method_not_allowed {
      let allow = self.allowed(path).join(", ");

      if !allow.is_empty() {
        if let Some(ref handler) = self.method_not_allowed {
          return handler.handle(req);
        }
        return Box::pin(async {
          Ok(
            Response::builder()
              .header(header::ALLOW, allow)
              .status(StatusCode::METHOD_NOT_ALLOWED)
              .body(Body::empty())
              .unwrap(),
          )
        });
      }
    };

    match &self.not_found {
      Some(handler) => handler.handle(req),
      None => Box::pin(async { Ok(Response::builder().status(404).body(Body::empty()).unwrap()) }),
    }
  }
}
