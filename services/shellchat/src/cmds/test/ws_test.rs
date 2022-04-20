mod ws_test_server;

use num_traits::{FromPrimitive, ToPrimitive};
use std::thread;
use websocket::{Opcode, Return, WebsocketConfig, CA_LEN, SERVER_NAME_WEBSOCKET};
use xous_ipc::Buffer;

const WS_TEST_NAME: &str = "_ws_test_";
const TEST_MSG_SIZE: usize = 128;
const PROTOCOL: &str = "echo";

#[derive(num_derive::FromPrimitive, num_derive::ToPrimitive, Debug)]
enum TestOpcode {
    Send,
    Receive,
    Quit,
}

pub fn local(tls: bool) {
    log::info!("Starting local websocket server");
    thread::spawn({
        move || {
            ws_test_server::main();
        }
    });
    log::info!("Started local websocket server on 127.0.0.1:1337");

    let ca = match tls {
        true => Some(xous_ipc::String::from_str(
            "TODO invalid certificate authority",
        )),
        false => None,
    };

    // start test_app
    thread::spawn({
        move || {
            test_app(ca);
        }
    });

    let tt = ticktimer_server::Ticktimer::new().unwrap();
    let xns = xous_names::XousNames::new().unwrap();

    // pause to allow test_app to get up and running
    tt.sleep_ms(1000).expect("insomnia");
    let test_app_cid = xns
        .request_connection_blocking(WS_TEST_NAME)
        .expect("Cannot connect to test_app");

    log::info!("Send TestOpcode::Send");
    xous::send_message(
        test_app_cid,
        xous::Message::new_scalar(TestOpcode::Send.to_usize().unwrap(), 0, 0, 0, 0),
    )
    .expect("failed to send test_app msg");

    // pause to allow local websocket server to echo messages
    tt.sleep_ms(5000).expect("insomnia");

    log::info!("Send TestOpcode::Quit (also triggers websocket close)");
    xous::send_message(
        test_app_cid,
        xous::Message::new_scalar(TestOpcode::Quit.to_usize().unwrap(), 0, 0, 0, 0),
    )
    .expect("failed to send test_app quit");
}

fn test_app(certificate_authority: Option<xous_ipc::String<CA_LEN>>) {
    log::info!("Starting Websocket test App");
    // register this test_app with xous
    let xns = xous_names::XousNames::new().unwrap();
    let sid = xns
        .register_name(WS_TEST_NAME, None)
        .expect("can't register server");
    log::trace!("registered with NS -- {:?}", sid);
    let cid: u32 = xous::connect(sid).unwrap();

    let ws_cid = xns
        .request_connection_blocking(SERVER_NAME_WEBSOCKET)
        .expect("Cannot connect to websocket server");
    let config = WebsocketConfig {
        certificate_authority: certificate_authority,
        base_url: xous_ipc::String::from_str("http://127.0.0.1:1337"),
        path: xous_ipc::String::from_str("/test"),
        use_credentials: false,
        sub_protocols: [
            xous_ipc::String::from_str(PROTOCOL),
            xous_ipc::String::new(),
            xous_ipc::String::new(),
        ],
        login: xous_ipc::String::from_str(""),
        password: xous_ipc::String::from_str(""),
        cid: cid,
        opcode: TestOpcode::Receive.to_u32().unwrap(),
    };
    log::info!("Opening websocket with {:?}", config);

    // Request the websocket_client_service to open a websocket with WebsocketConfig
    let mut buf = Buffer::into_buf(config)
        .or(Err(xous::Error::InternalError))
        .expect("failed to construct WebsocketConfig buffer");
    buf.lend_mut(ws_cid, Opcode::Open.to_u32().unwrap())
        .map(|_| ())
        .expect("request to open websocket failed");

    match buf.to_original::<Return, _>().unwrap() {
        Return::SubProtocol(protocol) => match protocol.to_str() {
            "echo" => log::info!("Opened WebSocket with protocol: {:?}", protocol.to_str()),
            _ => log::info!("FAIL: protocol != echo"),
        },
        Return::Failure(hint) => log::info!("FAIL: on retrieve protocol: {:?}", hint),
    };

    loop {
        let mut msg = xous::receive_message(sid).unwrap();
        match FromPrimitive::from_usize(msg.body.id()) {
            Some(TestOpcode::Send) => {
                log::info!("Received TestOpcode::Send");
                let outbound: xous_ipc::String<TEST_MSG_SIZE> = xous_ipc::String::from_str(
                    "send this message outbound from test_app via websocket",
                );
                let buf = Buffer::into_buf(outbound).expect("failed put msg in buffer");
                buf.send(ws_cid, Opcode::Send.to_u32().unwrap())
                    .map(|_| ())
                    .expect("failed to send via websocket");
                log::info!("Completed TestOpcode::Send");
            }
            Some(TestOpcode::Receive) => {
                log::info!("Received TestOpcode::Receive");
                let buf = unsafe {
                    Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())
                };
                let inbound = buf
                    .to_original::<xous_ipc::String<TEST_MSG_SIZE>, _>()
                    .unwrap();
                log::info!("Completed TestOpcode::Receive: {}", inbound);
            }
            Some(TestOpcode::Quit) => {
                log::info!("Received TestOpcode::Quit");
                break;
            }
            None => {
                log::error!("couldn't convert opcode: {:?}", msg);
            }
        }
    }

    log::info!("Closing websocket");
    xous::send_message(
        ws_cid,
        xous::Message::new_scalar(Opcode::Close.to_usize().unwrap(), 0, 0, 0, 0),
    )
    .expect("couldn't send test_app quit");
    log::info!("Closed websocket OK");
}
