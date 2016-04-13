use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::io::{Read, Write};
use std::time::Duration;

#[cfg(feature = "ssl")]
use std::path::PathBuf;

use time;
use num_cpus;

use url::percent_encoding::{percent_decode, percent_decode_to};
use url::{Url, SchemeData};

use hyper::{self, Encoder, Decoder, Next, Control};
use hyper::server::Handler as HyperHandler;
use hyper::server::Response as HyperResponse;
use hyper::server::{HandlerFactory, Request};
use hyper::header::{Date, ContentType};
use hyper::mime::Mime;
use hyper::uri::RequestUri;
use hyper::net::{HttpListener, Transport};
#[cfg(feature = "ssl")]
use hyper::net::{Openssl, HttpsListener};

pub use hyper::server::Listening;

use anymap::Map;
use anymap::any::Any;

use StatusCode;

use context::{RawContext, Uri, MaybeUtf8Owned, Parameters};
use filter::{FilterContext, ContextFilter, ContextAction, ResponseFilter};
use router::{Router, Endpoint};
use handler::{RawHandler, Factory};
use header::{Headers, HttpDate};
use server::{Scheme, Global};
use response::{RawResponse, ResponseHead};

use HttpResult;
use Server;

use utils;

struct Config<R: Router> {
    handlers: R,
    fallback_handler: Option<R::Handler>,

    host: SocketAddr,

    server: String,
    content_type: Mime,

    context_filters: Vec<Box<ContextFilter>>,
}

///A runnable instance of a server.
///
///It's not meant to be used directly,
///unless additional control is required.
///
///```no_run
///# use rustful::{Server, Handler, Context, Response};
///# #[derive(Default)]
///# struct R;
///# impl Handler for R {
///#     fn handle_request(&self, _context: Context, _response: Response) {}
///# }
///# let router = R;
///let (server_instance, scheme) = Server {
///    host: 8080.into(),
///    handlers: router,
///    ..Server::default()
///}.build();
///```
pub struct ServerInstance<R: Router> {
    config: Arc<Config<R>>,
    response_filters: Arc<Vec<Box<ResponseFilter>>>,
    global: Arc<Global>,
    threads: usize,
    keep_alive: bool,
    timeout: Duration,
    max_sockets: usize,
}

impl<R: Router> ServerInstance<R> {
    ///Create a new server instance, with the provided configuration. This is
    ///the same as `Server{...}.build()`.
    pub fn new(config: Server<R>) -> (ServerInstance<R>, Scheme) {
        (ServerInstance {
            config: Arc::new(Config {
                handlers: config.handlers,
                fallback_handler: config.fallback_handler,
                host: config.host.into(),
                server: config.server,
                content_type: config.content_type,
                context_filters: config.context_filters,
            }),
            response_filters: Arc::new(config.response_filters),
            global: Arc::new(config.global),
            threads: config.threads.unwrap_or_else(|| (num_cpus::get() * 5) / 4),
            keep_alive: config.keep_alive,
            timeout: config.timeout,
            max_sockets: config.max_sockets,
        },
        config.scheme)
    }

    ///Start the server.
    #[cfg(feature = "ssl")]
    pub fn run(self, scheme: Scheme) -> HttpResult<Listening> {
        let threads = self.threads;
        let server = match scheme {
            Scheme::Http => try!(HyperServer::http(&self.config.host)),
            Scheme::Https {cert, key} => try!(HyperServer::https(&self.config.host, cert, key)),
        };
        server.keep_alive(self.keep_alive)
            .timeout(self.timeout)
            .max_sockets(self.max_sockets)
            .run(self, threads)
    }

    ///Start the server.
    #[cfg(not(feature = "ssl"))]
    pub fn run(self, _scheme: Scheme) -> HttpResult<Listening> {
        let threads = self.threads;
        let server = try!(HyperServer::http(&self.config.host));
        server.keep_alive(self.keep_alive)
            .timeout(self.timeout)
            .max_sockets(self.max_sockets)
            .run(self, threads)
    }

}

struct ParsedUri {
    host: Option<(String, Option<u16>)>,
    uri: Uri,
    query: Parameters,
    fragment: Option<MaybeUtf8Owned>
}

impl<T: Transport, R: Router> HandlerFactory<T> for ServerInstance<R> where
    for<'a, 'b> &'a mut Encoder<'b, T>: Into<::handler::Encoder<'a, 'b>>,
    for<'a, 'b> &'a mut Decoder<'b, T>: Into<::handler::Decoder<'a, 'b>>,
{
    type Output = RequestHandler<R>;

    fn create(&mut self, control: Control) -> RequestHandler<R> {
        RequestHandler::new(self.config.clone(), self.response_filters.clone(), self.global.clone(), control)
    }
}

fn parse_path(path: &str) -> ParsedUri {
    match path.find('?') {
        Some(index) => {
            let (query, fragment) = parse_fragment(&path[index+1..]);

            let mut path = percent_decode(path[..index].as_bytes());
            if path.is_empty() {
                path.push(b'/');
            }

            ParsedUri {
                host: None,
                uri: Uri::Path(path.into()),
                query: utils::parse_parameters(query.as_bytes()),
                fragment: fragment.map(|f| percent_decode(f.as_bytes()).into())
            }
        },
        None => {
            let (path, fragment) = parse_fragment(&path);

            let mut path = percent_decode(path.as_bytes());
            if path.is_empty() {
                path.push(b'/');
            }

            ParsedUri {
                host: None,
                uri: Uri::Path(path.into()),
                query: Parameters::new(),
                fragment: fragment.map(|f| percent_decode(f.as_bytes()).into())
            }
        }
    }
}

fn parse_fragment(path: &str) -> (&str, Option<&str>) {
    match path.find('#') {
        Some(index) => (&path[..index], Some(&path[index+1..])),
        None => (path, None)
    }
}

fn parse_url(url: &Url) -> ParsedUri {
    let mut path = Vec::new();
    for component in url.path().unwrap_or(&[]) {
        path.push(b'/');
        percent_decode_to(component.as_bytes(), &mut path);
    }
    if path.is_empty() {
        path.push(b'/');
    }

    let query = url.query_pairs()
            .unwrap_or_default()
            .into_iter()
            .collect();

    let host = if let SchemeData::Relative(ref data) = url.scheme_data {
        Some((data.host.serialize(), data.port))
    } else {
        None
    };

    ParsedUri {
        host: host,
        uri: Uri::Path(path.into()),
        query: query,
        fragment: url.fragment.as_ref().map(|f| percent_decode(f.as_bytes()).into())
    }
}

//Helper to handle multiple protocols.
enum HyperServer {
    Http(hyper::server::Server<HttpListener>),
    #[cfg(feature = "ssl")]
    Https(hyper::server::Server<HttpsListener<Openssl>>),
}

impl HyperServer {
    fn http(host: &SocketAddr) -> HttpResult<HyperServer> {
        hyper::server::Server::http(host).map(HyperServer::Http)
    }

    #[cfg(feature = "ssl")]
    fn https(host: &SocketAddr, cert: PathBuf, key: PathBuf) -> HttpResult<HyperServer> {
        let ssl = try!(Openssl::server_with_cert_and_key(cert, key));
        hyper::server::Server::https(host, ssl).map(HyperServer::Https)
    }

    #[cfg(feature = "ssl")]
    fn keep_alive(self, enabled: bool) -> HyperServer {
        match self {
            HyperServer::Http(s) => HyperServer::Http(s.keep_alive(enabled)),
            HyperServer::Https(s) => HyperServer::Https(s.keep_alive(enabled)),
        }
    }

    #[cfg(not(feature = "ssl"))]
    fn keep_alive(self, enabled: bool) -> HyperServer {
        match self {
            HyperServer::Http(s) => HyperServer::Http(s.keep_alive(enabled)),
        }
    }

    #[cfg(feature = "ssl")]
    fn timeout(self, duration: Duration) -> HyperServer {
        match self {
            HyperServer::Http(s) => HyperServer::Http(s.idle_timeout(duration)),
            HyperServer::Https(s) => HyperServer::Https(s.idle_timeout(duration)),
        }
    }

    #[cfg(not(feature = "ssl"))]
    fn timeout(self, duration: Duration) -> HyperServer {
        match self {
            HyperServer::Http(s) => HyperServer::Http(s.idle_timeout(duration)),
        }
    }

    #[cfg(feature = "ssl")]
    fn max_sockets(self, max: usize) -> HyperServer {
        match self {
            HyperServer::Http(s) => HyperServer::Http(s.max_sockets(max)),
            HyperServer::Https(s) => HyperServer::Https(s.max_sockets(max)),
        }
    }

    #[cfg(not(feature = "ssl"))]
    fn max_sockets(self, max: usize) -> HyperServer {
        match self {
            HyperServer::Http(s) => HyperServer::Http(s.max_sockets(max)),
        }
    }

    #[cfg(feature = "ssl")]
    fn run<R: Router>(self, server: ServerInstance<R>, threads: usize) -> HttpResult<Listening> {
        match self {
            HyperServer::Http(s) => s.handle(server),
            HyperServer::Https(s) => s.handle(server),
        }
    }

    #[cfg(not(feature = "ssl"))]
    fn run<R: Router>(self, server: ServerInstance<R>, threads: usize) -> HttpResult<Listening> {
        match self {
            HyperServer::Http(s) => s.handle(server),
        }
    }
}

pub struct RequestHandler<R: Router> {
    config: Arc<Config<R>>,
    global: Arc<Global>,
    response_filters: Arc<Vec<Box<ResponseFilter>>>,
    write_method: Option<WriteMethod<<R::Handler as ::handler::Factory>::Handler>>,

    control: Option<Control>,
}

impl<R: Router> RequestHandler<R> {
    fn new(config: Arc<Config<R>>, response_filters: Arc<Vec<Box<ResponseFilter>>>, global: Arc<Global>, control: Control) -> RequestHandler<R> {
        RequestHandler {
            config: config,
            global: global,
            response_filters: response_filters,
            write_method: None,

            control: Some(control),
        }
    }
}

fn modify_context(context_filters: &[Box<ContextFilter>], global: &Global, filter_storage: &mut Map<Any + Send + 'static>, context: &mut RawContext) -> ContextAction {
    let mut result = ContextAction::Next;

    for filter in context_filters {
        result = match result {
            ContextAction::Next => {
                let filter_context = FilterContext {
                    storage: filter_storage,
                    global: global,
                };
                filter.modify(filter_context, context)
            },
            _ => return result
        };
    }

    result
}

impl<T: Transport, R: Router> HyperHandler<T> for RequestHandler<R> where
    for<'a, 'b> &'a mut Encoder<'b, T>: Into<::handler::Encoder<'a, 'b>>,
    for<'a, 'b> &'a mut Decoder<'b, T>: Into<::handler::Decoder<'a, 'b>>,
{
    fn on_request(&mut self, request: Request) -> Next {
        if let Some(control) = self.control.take() {
            let mut response = RawResponse {
                status: StatusCode::Ok,
                headers: Headers::new(),
                filters: ::interface::response::make_response_filters(
                    self.response_filters.clone(),
                    self.global.clone(),
                ),
            };

            response.headers.set(Date(HttpDate(time::now_utc())));
            response.headers.set(ContentType(self.config.content_type.clone()));
            response.headers.set(::header::Server(self.config.server.clone()));

            let path_components = match *request.uri() {
                RequestUri::AbsoluteUri(ref url) => Some(parse_url(url)),
                RequestUri::AbsolutePath(ref path) => Some(parse_path(path)),
                RequestUri::Star => {
                    Some(ParsedUri {
                        host: None,
                        uri: Uri::Asterisk,
                        query: Parameters::new(),
                        fragment: None
                    })
                },
                _ => None
            };

            let (write_method, next) = match path_components {
                Some(ParsedUri{ host, uri, query, fragment }) => {
                    /*if let Some((name, port)) = host {
                        request_headers.set(::header::Host {
                            hostname: name,
                            port: port
                        });
                    }*/

                    let mut context = RawContext {
                        request: request,
                        uri: uri,
                        hyperlinks: vec![],
                        variables: Parameters::new(),
                        query: query,
                        fragment: fragment,
                        global: self.global.clone(),
                        control: control,
                    };

                    let mut filter_storage = Map::new();

                    match modify_context(&self.config.context_filters, &self.global, &mut filter_storage, &mut context) {
                        ContextAction::Next => {
                            response.filters.storage = filter_storage;
                            let config = &self.config;

                            let endpoint = context.uri.as_path().map_or_else(|| {
                                Endpoint {
                                    handler: None,
                                    variables: HashMap::new(),
                                    hyperlinks: vec![]
                                }
                            }, |path| config.handlers.find(&context.request.method(), &mut (&path[..]).into()));

                            let Endpoint {
                                handler,
                                variables,
                                hyperlinks
                            } = endpoint;

                            if let Some(handler) = handler.or(config.fallback_handler.as_ref()) {
                                context.hyperlinks = hyperlinks;
                                context.variables = variables.into();
                                let mut handler = handler.create(context, response);
                                let next = handler.on_request();
                                (WriteMethod::Handler(handler), next)
                            } else {
                                response.status = StatusCode::NotFound;
                                (
                                    WriteMethod::Error(Some(ResponseHead {
                                        status: response.status,
                                        headers: response.headers,
                                    })),
                                    Next::write()
                                )
                            }
                        },
                        ContextAction::Abort(status) => {
                            response.status = status;
                            (
                                WriteMethod::Error(Some(ResponseHead {
                                    status: response.status,
                                    headers: response.headers,
                                })),
                                Next::write()
                            )
                        }
                    }
                },
                None => {
                    response.status = StatusCode::BadRequest;
                    (
                        WriteMethod::Error(Some(ResponseHead {
                            status: response.status,
                            headers: response.headers,
                        })),
                        Next::write()
                    )
                }
            };

            self.write_method = Some(write_method);
            next
        } else {
            panic!("RequestHandler reused");
        }
    }

    fn on_request_readable(&mut self, decoder: &mut Decoder<T>) -> Next {
        if let Some(WriteMethod::Handler(ref mut handler)) = self.write_method {
            handler.on_request_readable(decoder.into())
        } else {
            Next::write()
        }
    }

    fn on_response(&mut self, response: &mut HyperResponse) -> Next {
        if let Some(ref mut method) = self.write_method {
            let (head, next) = match *method {
                WriteMethod::Handler(ref mut handler) => handler.on_response(),
                WriteMethod::Error(ref mut head) => (head.take().expect("missing response head"), Next::end()),
            };

            response.set_status(head.status);
            response.headers_mut().extend(head.headers.iter());

            next
        } else {
            panic!("missing write method")
        }
    }

    fn on_response_writable(&mut self, encoder: &mut Encoder<T>) -> Next {
        if let Some(WriteMethod::Handler(ref mut handler)) = self.write_method {
            handler.on_response_writable(encoder.into())
        } else {
            Next::end()
        }
    }
}

enum WriteMethod<H> {
    Handler(H),
    Error(Option<ResponseHead>),
}


#[test]
fn parse_path_parts() {
    let with = "this".to_owned().into();
    let and = "that".to_owned().into();
    let ParsedUri { uri, query, fragment, .. } = parse_path("/path/to/something?with=this&and=that#lol");
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, Some("lol".to_owned().into()));
}

#[test]
fn parse_strange_path() {
    let with = "this".to_owned().into();
    let and = "what?".to_owned().into();
    let ParsedUri { uri, query, fragment, .. } = parse_path("/path/to/something?with=this&and=what?#");
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, Some(String::new().into()));
}

#[test]
fn parse_missing_path_parts() {
    let with = "this".to_owned().into();
    let and = "that".to_owned().into();
    let ParsedUri { uri, query, fragment, .. } = parse_path("/path/to/something?with=this&and=that");
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, None);


    let ParsedUri { uri, query, fragment, .. } = parse_path("/path/to/something#lol");
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.len(), 0);
    assert_eq!(fragment, Some("lol".to_owned().into()));


    let ParsedUri { uri, query, fragment, .. } = parse_path("?with=this&and=that#lol");
    assert_eq!(uri.as_path(), Some("/".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, Some("lol".to_owned().into()));
}


#[test]
fn parse_url_parts() {
    let with = "this".to_owned().into();
    let and = "that".to_owned().into();
    let url = Url::parse("http://example.com/path/to/something?with=this&and=that#lol").unwrap();
    let ParsedUri { uri, query, fragment, .. } = parse_url(&url);
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, Some("lol".to_owned().into()));
}

#[test]
fn parse_strange_url() {
    let with = "this".to_owned().into();
    let and = "what?".to_owned().into();
    let url = Url::parse("http://example.com/path/to/something?with=this&and=what?#").unwrap();
    let ParsedUri { uri, query, fragment, .. } = parse_url(&url);
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, Some(String::new().into()));
}

#[test]
fn parse_missing_url_parts() {
    let with = "this".to_owned().into();
    let and = "that".to_owned().into();
    let url = Url::parse("http://example.com/path/to/something?with=this&and=that").unwrap();
    let ParsedUri { uri, query, fragment, .. } = parse_url(&url);
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, None);


    let url = Url::parse("http://example.com/path/to/something#lol").unwrap();
    let ParsedUri { uri, query, fragment, .. } = parse_url(&url);
    assert_eq!(uri.as_path(), Some("/path/to/something".into()));
    assert_eq!(query.len(), 0);
    assert_eq!(fragment, Some("lol".to_owned().into()));


    let url = Url::parse("http://example.com?with=this&and=that#lol").unwrap();
    let ParsedUri { uri, query, fragment, .. } = parse_url(&url);
    assert_eq!(uri.as_path(), Some("/".into()));
    assert_eq!(query.get_raw("with"), Some(&with));
    assert_eq!(query.get_raw("and"), Some(&and));
    assert_eq!(fragment, Some("lol".to_owned().into()));
}
