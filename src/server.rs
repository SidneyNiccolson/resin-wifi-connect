use std::sync::mpsc::{Receiver, Sender};
use std::fmt;
use std::net::Ipv4Addr;
use std::error::Error as StdError;

use serde_json;
use path::PathBuf;
use iron::prelude::*;
use iron::{headers, status, typemap, AfterMiddleware, Iron, IronError, IronResult, Request,
           Response, Url};
use iron::modifiers::Redirect;
use iron_cors::CorsMiddleware;
use router::Router;
use staticfile::Static;
use mount::Mount;
use persistent::Write;
use params::{FromValue, Params};

use errors::*;
use network::{NetworkCommand, NetworkCommandResponse};
use exit::{exit, ExitResult};



use std::fs::File;
use std::path::Path;
use std::io::Write as IoWrite;

struct RequestSharedState {
    gateway: Ipv4Addr,
    server_rx: Receiver<NetworkCommandResponse>,
    network_tx: Sender<NetworkCommand>,
    exit_tx: Sender<ExitResult>,
}

impl typemap::Key for RequestSharedState {
    type Value = RequestSharedState;
}

#[derive(Debug)]
struct StringError(String);

impl fmt::Display for StringError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl StdError for StringError {
    fn description(&self) -> &str {
        &*self.0
    }
}

macro_rules! get_request_ref {
    ($req:ident, $ty:ty, $err:expr) => (
        match $req.get_ref::<$ty>() {
            Ok(val) => val,
            Err(err) => {
                error!($err);
                return Err(IronError::new(err, status::InternalServerError));
            }
        }
    )
}

macro_rules! get_param {
    ($params:ident, $param:expr, $ty:ty) => (
        match $params.get($param) {
            Some(value) => {
                match <$ty as FromValue>::from_value(value) {
                    Some(converted) => converted,
                    None => {
                        let err = format!("Unexpected type for '{}'", $param);
                        error!("{}", err);
                        return Err(IronError::new(StringError(err), status::InternalServerError));
                    }
                }
            },
            None => {
                let err = format!("'{}' not found in request params: {:?}", $param, $params);
                error!("{}", err);
                return Err(IronError::new(StringError(err), status::InternalServerError));
            }
        }
    )
}

macro_rules! get_request_state {
    ($req:ident) => (
        get_request_ref!(
            $req,
            Write<RequestSharedState>,
            "Getting reference to request shared state failed"
        ).as_ref().lock().unwrap()
    )
}

fn exit_with_error<E>(state: &RequestSharedState, e: E, e_kind: ErrorKind) -> IronResult<Response>
where
    E: ::std::error::Error + Send + 'static,
{
    let description = e_kind.description().into();
    let err = Err::<Response, E>(e).chain_err(|| e_kind);
    exit(&state.exit_tx, err.unwrap_err());
    Err(IronError::new(
        StringError(description),
        status::InternalServerError,
    ))
}

struct RedirectMiddleware;

impl AfterMiddleware for RedirectMiddleware {
    fn catch(&self, req: &mut Request, err: IronError) -> IronResult<Response> {
        let gateway = {
            let request_state = get_request_state!(req);
            format!("{}", request_state.gateway)
        };

        if let Some(host) = req.headers.get::<headers::Host>() {
            if host.hostname != gateway {
                let url = Url::parse(&format!("http://{}/", gateway)).unwrap();
                return Ok(Response::with((status::Found, Redirect(url))));
            }
        }

        Err(err)
    }
}

pub fn start_server(
    gateway: Ipv4Addr,
    listening_port: u16,
    server_rx: Receiver<NetworkCommandResponse>,
    network_tx: Sender<NetworkCommand>,
    exit_tx: Sender<ExitResult>,
    ui_directory: &PathBuf,
) {
    let exit_tx_clone = exit_tx.clone();
    let gateway_clone = gateway;
    let request_state = RequestSharedState {
        gateway: gateway,
        server_rx: server_rx,
        network_tx: network_tx,
        exit_tx: exit_tx,
    };

    let mut router = Router::new();
    router.get("/", Static::new(ui_directory), "index");
    router.get("/ssid", ssid, "ssid");
    router.post("/connect", connect, "connect");

    let mut assets = Mount::new();
    assets.mount("/", router);
    assets.mount("/css", Static::new(&ui_directory.join("css")));
    assets.mount("/img", Static::new(&ui_directory.join("img")));
    assets.mount("/js", Static::new(&ui_directory.join("js")));

    let cors_middleware = CorsMiddleware::with_allow_any();

    let mut chain = Chain::new(assets);
    chain.link(Write::<RequestSharedState>::both(request_state));
    chain.link_after(RedirectMiddleware);
    chain.link_around(cors_middleware);

    let address = format!("{}:{}", gateway_clone, listening_port);

    info!("Starting HTTP server on {}", &address);

    if let Err(e) = Iron::new(chain).http(&address) {
        exit(
            &exit_tx_clone,
            ErrorKind::StartHTTPServer(address, e.description().into()).into(),
        );
    }
}

fn ssid(req: &mut Request) -> IronResult<Response> {
    info!("User connected to the captive portal");

    let request_state = get_request_state!(req);

    if let Err(e) = request_state.network_tx.send(NetworkCommand::Activate) {
        return exit_with_error(&request_state, e, ErrorKind::SendNetworkCommandActivate);
    }

    let access_points_ssids = match request_state.server_rx.recv() {
        Ok(result) => match result {
            NetworkCommandResponse::AccessPointsSsids(ssids) => ssids,
        },
        Err(e) => return exit_with_error(&request_state, e, ErrorKind::RecvAccessPointSSIDs),
    };

    let access_points_json = match serde_json::to_string(&access_points_ssids) {
        Ok(json) => json,
        Err(e) => return exit_with_error(&request_state, e, ErrorKind::SerializeAccessPointSSIDs),
    };

    Ok(Response::with((status::Ok, access_points_json)))
}

fn connect(req: &mut Request) -> IronResult<Response> {
    let (ssid, passphrase, API, WS, serial, secret) = {
        let params = get_request_ref!(req, Params, "Getting request params failed");
        let ssid = get_param!(params, "ssid", String);
        let passphrase = get_param!(params, "passphrase", String);
        let API = get_param!(params, "API", String);
        let WS = get_param!(params, "WS", String);
        let serial = get_param!(params, "serial", String);
        let secret = get_param!(params, "secret", String);
        (ssid, passphrase, API, WS, serial, secret)
    };
    // Handle writing files
    let json_path = Path::new("/data/kit_config.json");
    let json_display = json_path.display();

    // Open a file in write-only mode, returns `io::Result<File>`
    let mut file = match File::create(&json_path) {
        Err(why) => panic!("couldn't create {}: {}",
                           json_display,
                           why.description()),
        Ok(file) => file,
    };


    let output_text = format!("{{
    \"api\": {{
        \"root\": \"{}\"
    }},
    \"websockets\": {{
        \"url\": \"{}\"
    }},
    \"auth\": {{
        \"serial\": \"{}\",
        \"secret\": \"{}\"
    }},
    \"debug\": {{
		\"level\": \"INFO\",
		\"peripheral_display\": {{
		    \"module_name\": \"astroplant_peripheral_device_library.lcd\",
		    \"class_name\": \"LCD\",
		    \"parameters\": {{
		        \"i2c_address\": \"0x3f\"
		    }}
		}}
	    }}
	}}", API, WS, serial, secret);

    // Write the text string to `file`, returns `io::Result<()>`
    match file.write_all(output_text.as_bytes())
    {
        Err(why) => {
            panic!("couldn't write to {}: {}", json_display,
                                               why.description())
        },
        Ok(_) => println!("successfully wrote to {}", json_display),
    }

    debug!("Incoming `connect` to access point `{}` request", ssid);

    let request_state = get_request_state!(req);

    let command = NetworkCommand::Connect {
        ssid: ssid,
        passphrase: passphrase,
    };

    if let Err(e) = request_state.network_tx.send(command) {
        exit_with_error(&request_state, e, ErrorKind::SendNetworkCommandConnect)
    } else {
        Ok(Response::with(status::Ok))
    }
}
