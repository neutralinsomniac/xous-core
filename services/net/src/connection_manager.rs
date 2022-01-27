use crate::api::*;
use std::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use com::{WlanStatus, WlanStatusIpc};
use com_rs_ref::{ConnectResult, LinkState};
use net::MIN_EC_REV;
use xous::{msg_blocking_scalar_unpack, msg_scalar_unpack, send_message, try_send_message, Message};
use xous_ipc::Buffer;
use num_traits::*;
use std::io::Read;
use std::collections::{HashMap, HashSet};
use locales::t;
use crate::ComIntSources;

#[allow(dead_code)]
const BOOT_POLL_INTERVAL_MS: usize = 3_758; // a slightly faster poll during boot so we acquire wifi faster once PDDB is mounted
/// this is shared externally so other functions (e.g. in status bar) that want to query the net manager know how long to back off, otherwise the status query will block
#[allow(dead_code)]
const POLL_INTERVAL_MS: usize = 10_151; // stagger slightly off of an integer-seconds interval to even out loads. impacts rssi update frequency.
const INTERVALS_BEFORE_RETRY: usize =  3; // how many poll intervals we'll wait before we give up and try a new AP

#[derive(num_derive::FromPrimitive, num_derive::ToPrimitive, Debug)]
pub(crate) enum ConnectionManagerOpcode {
    Run,
    Poll,
    Stop,
    SubscribeWifiStats,
    UnsubWifiStats,
    ComInt,
    SuspendResume,
    Quit,
}
#[derive(num_derive::FromPrimitive, num_derive::ToPrimitive, Debug)]
enum PumpOp {
    Pump,
    Quit,
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
enum WifiState {
    Unknown,
    Connecting,
    WaitDhcp,
    Retry,
    InvalidAp,
    InvalidAuth,
    Connected,
    Disconnected,
    Error
}
#[derive(Eq, PartialEq)]
enum SsidScanState {
    Idle,
    Scanning,
}

pub(crate) fn connection_manager(sid: xous::SID, activity_interval: Arc<AtomicU32>) {
    let tt = ticktimer_server::Ticktimer::new().unwrap();
    let xns = xous_names::XousNames::new().unwrap();
    let mut com = com::Com::new(&xns).unwrap();
    let netmgr = net::NetManager::new();
    let mut pddb = pddb::Pddb::new();
    let self_cid = xous::connect(sid).unwrap();
    // give the system some time to boot before trying to run a check on the EC minimum version, as it is in reset on boot
    tt.sleep_ms(POLL_INTERVAL_MS).unwrap();
    let modals = modals::Modals::new(&xns).unwrap();

    // check that the EC rev meets the minimum version for this service to function
    // otherwise, we could crash the EC before it can update itself.
    let (maj, min, rev, commits) = com.get_ec_sw_tag().unwrap();
    let ec_rev = (maj as u32) << 24 | (min as u32) << 16 | (rev as u32) << 8 | commits as u32;
    let rev_ok = ec_rev >= MIN_EC_REV;
    if !rev_ok {
        log::warn!("EC firmware is too old to interoperate with the connection manager.");
        let mut note = String::from(t!("net.ec_rev_old", xous::LANG));
        note.push_str(&format!("\n\n{}{}.{}.{}+{}", t!("net.ec_current_rev", xous::LANG), maj, min, rev, commits));
        modals.show_notification(&note).unwrap();
    }

    let run = Arc::new(AtomicBool::new(rev_ok));
    let pumping = Arc::new(AtomicBool::new(false));
    let mut mounted = false;
    let current_interval = Arc::new(AtomicU32::new(BOOT_POLL_INTERVAL_MS as u32));
    let mut wifi_stats_cache: WlanStatus = WlanStatus::from_ipc(WlanStatusIpc::default());
    let mut status_subscribers = HashMap::<xous::CID, WifiStateSubscription>::new();
    let mut wifi_state = WifiState::Unknown;
    let mut last_wifi_state = wifi_state;
    let mut ssid_list = HashSet::<String>::new(); // we're throwing away the RSSI for now and just going by name
    let mut ssid_attempted = HashSet::<String>::new();
    let mut wait_count = 0;

    let run_sid = xous::create_server().unwrap();
    let run_cid = xous::connect(run_sid).unwrap();
    let _ = std::thread::spawn({
        let run = run.clone();
        let sid = run_sid.clone();
        let main_cid = self_cid.clone();
        let self_cid = run_cid.clone();
        let interval = current_interval.clone();
        let pumping = pumping.clone();
        move || {
            let tt = ticktimer_server::Ticktimer::new().unwrap();
            loop {
                let msg = xous::receive_message(sid).unwrap();
                match FromPrimitive::from_usize(msg.body.id()) {
                    Some(PumpOp::Pump) => msg_scalar_unpack!(msg, _, _, _, _, {
                        if run.load(Ordering::SeqCst) {
                            pumping.store(true, Ordering::SeqCst);
                            try_send_message(main_cid, Message::new_scalar(ConnectionManagerOpcode::Poll.to_usize().unwrap(), 0, 0, 0, 0)).ok();
                            tt.sleep_ms(interval.load(Ordering::SeqCst) as usize).unwrap();
                            send_message(self_cid, Message::new_scalar(PumpOp::Pump.to_usize().unwrap(), 0, 0, 0, 0)).unwrap();
                            pumping.store(false, Ordering::SeqCst);
                        }
                    }),
                    Some(PumpOp::Quit) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                        xous::return_scalar(msg.sender, 1).ok();
                        break;
                    }),
                    _ => log::error!("Unrecognized message: {:?}", msg),
                }
            }
            xous::destroy_server(sid).unwrap();
        }
    });

    let mut susres = susres::Susres::new(
        Some(susres::SuspendOrder::Early), &xns,
        ConnectionManagerOpcode::SuspendResume as u32, self_cid).expect("couldn't create suspend/resume object");

    com.set_ssid_scanning(true).unwrap(); // kick off an initial SSID scan, we'll always want this info regardless
    let mut scan_state = SsidScanState::Scanning;

    send_message(run_cid, Message::new_scalar(PumpOp::Pump.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't kick off next poll");
    loop {
        let msg = xous::receive_message(sid).unwrap();
        log::debug!("got msg: {:?}", msg);
        match FromPrimitive::from_usize(msg.body.id()) {
            Some(ConnectionManagerOpcode::SuspendResume) => xous::msg_scalar_unpack!(msg, token, _, _, _, {
                // for now, nothing to do to prepare for suspend...
                susres.suspend_until_resume(token).expect("couldn't execute suspend/resume");
                // on resume, check with the EC and see where the link state ended up
                let (res_linkstate, _res_dhcpstate) = com.wlan_sync_state().unwrap();
                match res_linkstate {
                    LinkState::Connected => {
                        match wifi_state {
                            WifiState::Connected => {
                                // everything is A-OK
                            },
                            WifiState::Error => {
                                // let the error handler do its thing on the next pump cycle
                            }
                            _ => {
                                // somehow, we thought we were disconnected, but then we resumed and we're magically connected.
                                // it's not clear to me how we get into this state, so let's be conservative and just leave the link and restart things.
                                com.wlan_leave().expect("couldn't issue leave command"); // leave the previous config to reset state
                                netmgr.reset();
                                wifi_state = WifiState::Disconnected;
                                if scan_state == SsidScanState::Idle {
                                    com.set_ssid_scanning(true).unwrap();
                                    scan_state = SsidScanState::Scanning;
                                }
                                send_message(self_cid, Message::new_scalar(ConnectionManagerOpcode::Poll.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't kick off next poll");
                            }
                        }
                    }
                    LinkState::WFXError => {
                        wifi_state = WifiState::Error;
                    }
                    _ => { // should approximately be a "disconnected" state.
                        match wifi_state {
                            WifiState::Connected => {
                                // move the wifi into the disconnected state to re-initiate a connection
                                netmgr.reset();
                                // reset the stats cache, and update subscribers that we're disconnected
                                wifi_stats_cache = WlanStatus::from_ipc(WlanStatusIpc::default());
                                for &sub in status_subscribers.keys() {
                                    let buf = Buffer::into_buf(com::WlanStatusIpc::from_status(wifi_stats_cache)).or(Err(xous::Error::InternalError)).unwrap();
                                    buf.send(sub, WifiStateCallback::Update.to_u32().unwrap()).or(Err(xous::Error::InternalError)).unwrap();
                                }
                                wifi_state = WifiState::Disconnected;
                                // kick off an SSID scan
                                if scan_state == SsidScanState::Idle {
                                    com.set_ssid_scanning(true).unwrap();
                                    scan_state = SsidScanState::Scanning;
                                }
                            }
                            WifiState::Error => {
                                // let the error handler do its thing on the next pump cycle
                            }
                            _ => {
                                // we were in some intermediate state, just "snap" us to disconnected and let the state machine take care of the rest
                                wifi_state = WifiState::Disconnected;
                            }
                        }
                    }
                }
            }),
            Some(ConnectionManagerOpcode::ComInt) => msg_scalar_unpack!(msg, ints, raw_arg, 0, 0, {
                log::debug!("debug: {:x}, {:x}", ints, raw_arg);
                let mut mask_bit: u16 = 1;
                for _ in 0..16 {
                    match ComIntSources::from(mask_bit & (ints as u16)) {
                        ComIntSources::Connect => {
                            wifi_state = match ConnectResult::decode_u16(raw_arg as u16) {
                                ConnectResult::Success => {
                                    com.set_ssid_scanning(false).unwrap();
                                    scan_state = SsidScanState::Idle;
                                    activity_interval.store(0, Ordering::SeqCst);
                                    WifiState::WaitDhcp
                                },
                                ConnectResult::NoMatchingAp => WifiState::InvalidAp,
                                ConnectResult::Timeout => WifiState::Retry,
                                ConnectResult::Reject | ConnectResult::AuthFail => WifiState::InvalidAuth,
                                ConnectResult::Aborted => WifiState::Retry,
                                ConnectResult::Error => WifiState::Error,
                                ConnectResult::Pending => WifiState::Error,
                            };
                            log::debug!("comint new wifi state: {:?}", wifi_state);
                        }
                        ComIntSources::Disconnect => {
                            ssid_list.clear(); // clear the ssid list because a likely cause of disconnect is we've moved out of range
                            com.set_ssid_scanning(true).unwrap();
                            scan_state = SsidScanState::Scanning;
                            wifi_state = WifiState::Disconnected;
                        },
                        ComIntSources::WlanSsidScanUpdate => {
                            // aggressively pre-fetch results so we can connect as soon as we see an SSID
                            match com.ssid_fetch_as_list() {
                                Ok(slist) => {
                                    for (_rssi, ssid) in slist.iter() {
                                        ssid_list.insert(ssid.to_string());
                                    }
                                },
                                _ => continue,
                            }
                            log::debug!("ssid scan update");
                        },
                        ComIntSources::WlanSsidScanFinished => {
                            match com.ssid_fetch_as_list() {
                                Ok(slist) => {
                                    for (_rssi, ssid) in slist.iter() {
                                        ssid_list.insert(ssid.to_string());
                                    }
                                },
                                _ => continue,
                            }
                            scan_state = SsidScanState::Idle;
                        }
                        ComIntSources::WlanIpConfigUpdate => {
                            activity_interval.store(0, Ordering::SeqCst);
                            wifi_state = WifiState::Connected;
                            log::debug!("comint new wifi state: {:?}", wifi_state);
                            // this is the "first" path -- it's hit immediately on connect.
                            // relay status updates to any subscribers that want to know if a state has changed
                            wifi_stats_cache = com.wlan_status().unwrap();
                            log::debug!("stats update: {:?}", wifi_stats_cache);
                            for &sub in status_subscribers.keys() {
                                let buf = Buffer::into_buf(com::WlanStatusIpc::from_status(wifi_stats_cache)).or(Err(xous::Error::InternalError)).unwrap();
                                buf.send(sub, WifiStateCallback::Update.to_u32().unwrap()).or(Err(xous::Error::InternalError)).unwrap();
                            }
                        }
                        _ => {}
                    }
                    mask_bit <<= 1;
                }
            }),
            Some(ConnectionManagerOpcode::Poll) => msg_scalar_unpack!(msg, _, _, _, _, {
                // heh. this probably should be rewritten to be a bit more thread-safe if we had a multi-core CPU we're running on. but we're single-core so...
                if activity_interval.fetch_add(current_interval.load(Ordering::SeqCst) as u32, Ordering::SeqCst) > current_interval.load(Ordering::SeqCst) as u32 {
                    log::info!("wlan activity interval timeout");
                    // if the pddb isn't mounted, don't even bother checking -- we can't connect until we have a place to get keys
                    if pddb.is_mounted() && rev_ok {
                        mounted = true;

                        if last_wifi_state == WifiState::Connected && wifi_state != WifiState::Connected {
                            log::debug!("sending disconnect update to subscribers");
                            // reset the stats cache, and update subscribers that we're disconnected
                            wifi_stats_cache = WlanStatus::from_ipc(WlanStatusIpc::default());
                            for &sub in status_subscribers.keys() {
                                let buf = Buffer::into_buf(com::WlanStatusIpc::from_status(wifi_stats_cache)).or(Err(xous::Error::InternalError)).unwrap();
                                buf.send(sub, WifiStateCallback::Update.to_u32().unwrap()).or(Err(xous::Error::InternalError)).unwrap();
                            }
                        }

                        if let Ok(ap_list_vec) = pddb.list_keys(AP_DICT_NAME, None) {
                            let mut ap_list = HashSet::<String>::new();
                            for ap in ap_list_vec {
                                ap_list.insert(ap);
                            }
                            match wifi_state {
                                WifiState::Unknown | WifiState::Disconnected | WifiState::InvalidAp | WifiState::InvalidAuth => {
                                    if scan_state == SsidScanState::Scanning {
                                        com.set_ssid_scanning(false).unwrap();
                                        scan_state = SsidScanState::Idle;
                                    }
                                    if let Some(ssid) = get_next_ssid(&mut ssid_list, &mut ssid_attempted, ap_list) {
                                        let mut wpa_pw_file = pddb.get(AP_DICT_NAME, &ssid, None, false, false, None, Some(||{})).expect("couldn't retrieve AP password");
                                        let mut wp_pw_raw = [0u8; com::api::WF200_PASS_MAX_LEN];
                                        if let Ok(readlen) = wpa_pw_file.read(&mut wp_pw_raw) {
                                            let pw = std::str::from_utf8(&wp_pw_raw[..readlen]).expect("password was not valid utf-8");
                                            log::info!("Attempting wifi connection: {}", ssid);
                                            com.wlan_set_ssid(&ssid).expect("couldn't set SSID");
                                            com.wlan_set_pass(pw).expect("couldn't set password");
                                            com.wlan_join().expect("couldn't issue join command");
                                            wifi_state = WifiState::Connecting;
                                        }
                                    }
                                }
                                WifiState::WaitDhcp | WifiState::Connecting => {
                                    log::debug!("still waiting for connection result...");
                                    wait_count += 1;
                                    if wait_count > INTERVALS_BEFORE_RETRY {
                                        wait_count = 0;
                                        wifi_state = WifiState::Retry;
                                    }
                                }
                                WifiState::Retry => {
                                    log::debug!("got Retry on connect");
                                    com.wlan_leave().expect("couldn't issue leave command"); // leave the previous config to reset state
                                    netmgr.reset();
                                    wifi_state = WifiState::Disconnected;
                                    if scan_state == SsidScanState::Idle {
                                        com.set_ssid_scanning(true).unwrap();
                                        scan_state = SsidScanState::Scanning;
                                    }
                                    send_message(self_cid, Message::new_scalar(ConnectionManagerOpcode::Poll.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't kick off next poll");
                                }
                                WifiState::Error => {
                                    log::debug!("got error on connect, resetting wifi chip");
                                    com.wifi_reset().expect("couldn't reset the wf200 chip");
                                    netmgr.reset(); // this can result in a suspend failure, but the suspend timeout is currently set long enough to accommodate this possibility
                                    wifi_state = WifiState::Disconnected;
                                    if scan_state == SsidScanState::Idle {
                                        com.set_ssid_scanning(true).unwrap();
                                        scan_state = SsidScanState::Scanning;
                                    }
                                    send_message(self_cid, Message::new_scalar(ConnectionManagerOpcode::Poll.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't kick off next poll");
                                }
                                WifiState::Connected => {
                                    // this is the "rare" path -- it's if we connected and not much is going on, so we timeout and hit this ping
                                    log::debug!("connected, updating stats cache");
                                    // relay status updates to any subscribers that want to know if a state has changed
                                    wifi_stats_cache = com.wlan_status().unwrap();
                                    log::debug!("stats update: {:?}", wifi_stats_cache);
                                    for &sub in status_subscribers.keys() {
                                        let buf = Buffer::into_buf(com::WlanStatusIpc::from_status(wifi_stats_cache)).or(Err(xous::Error::InternalError)).unwrap();
                                        buf.send(sub, WifiStateCallback::Update.to_u32().unwrap()).or(Err(xous::Error::InternalError)).unwrap();
                                    }
                                }
                            }
                        }
                    }
                    last_wifi_state = wifi_state;
                }

                if wifi_state == WifiState::Connected {
                    if let Some(ssid_stats) = wifi_stats_cache.ssid.as_mut() {
                        let rssi_u8 = com.wlan_get_rssi().ok().unwrap_or(255);
                        // only send an update if the RSSI changed
                        if ssid_stats.rssi != rssi_u8 {
                            ssid_stats.rssi = rssi_u8;
                            log::debug!("stats update: {:?}", wifi_stats_cache);
                            for &sub in status_subscribers.keys() {
                                let buf = Buffer::into_buf(com::WlanStatusIpc::from_status(wifi_stats_cache)).or(Err(xous::Error::InternalError)).unwrap();
                                buf.send(sub, WifiStateCallback::Update.to_u32().unwrap()).or(Err(xous::Error::InternalError)).unwrap();
                            }
                        }
                    }
                }

                if !mounted {
                    current_interval.store(BOOT_POLL_INTERVAL_MS as u32, Ordering::SeqCst);
                } else {
                    current_interval.store(POLL_INTERVAL_MS as u32, Ordering::SeqCst);
                }
            }),
            Some(ConnectionManagerOpcode::SubscribeWifiStats) => {
                let buffer = unsafe {
                    Buffer::from_memory_message(msg.body.memory_message().unwrap())
                };
                let sub = buffer.to_original::<WifiStateSubscription, _>().unwrap();
                let sub_cid = xous::connect(xous::SID::from_array(sub.sid)).expect("couldn't connect to wifi subscriber callback");
                status_subscribers.insert(sub_cid, sub);
            },
            Some(ConnectionManagerOpcode::UnsubWifiStats) => msg_blocking_scalar_unpack!(msg, s0, s1, s2, s3, {
                // note: this routine largely untested, could have some errors around the ordering of the blocking return vs the disconnect call.
                let sid = [s0 as u32, s1 as u32, s2 as u32, s3 as u32];
                let mut valid_sid: Option<xous::CID> = None;
                for (&cid, &sub) in status_subscribers.iter() {
                    if sub.sid == sid {
                        valid_sid = Some(cid)
                    }
                }
                xous::return_scalar(msg.sender, 1).expect("couldn't ack unsub");
                if let Some(cid) = valid_sid {
                    status_subscribers.remove(&cid);
                    unsafe{xous::disconnect(cid).expect("couldn't remove wifi status subscriber from our CID list that is limited to 32 items total. Suspect issue with ordering of disconnect vs blocking return...");}
                }
            }),
            Some(ConnectionManagerOpcode::Run) => msg_scalar_unpack!(msg, _, _, _, _, {
                if !run.swap(true, Ordering::SeqCst) {
                    if !pumping.load(Ordering::SeqCst) { // avoid having multiple pump messages being sent if a user tries to rapidly toggle the run/stop switch
                        send_message(run_cid, Message::new_scalar(PumpOp::Pump.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't kick off next poll");
                    }
                }
            }),
            Some(ConnectionManagerOpcode::Stop) => msg_scalar_unpack!(msg, _, _, _, _, {
                run.store(false, Ordering::SeqCst);
            }),
            Some(ConnectionManagerOpcode::Quit) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                send_message(run_cid, Message::new_blocking_scalar(PumpOp::Quit.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't tell Pump to quit");
                unsafe{xous::disconnect(run_cid).ok()};
                xous::return_scalar(msg.sender, 0).unwrap();
                log::warn!("exiting connection manager");
                break;
            }),
            None => {
                log::error!("couldn't convert opcode: {:?}", msg);
            }
        }
    }
    unsafe{xous::disconnect(self_cid).ok()};
    xous::destroy_server(sid).unwrap();
}

fn get_next_ssid(ssid_list: &mut HashSet<String>, ssid_attempted: &mut HashSet<String>, ap_list: HashSet::<String>) -> Option<String> {
    log::trace!("ap_list: {:?}", ap_list);
    log::trace!("ssid_list: {:?}", ssid_list);
    // 1. find the intersection of ap_list and ssid_list to create a candidate_list
    let all_candidate_list_ref = ap_list.intersection(ssid_list).collect::<HashSet<_>>();
    // this copy is required to perform the next set computation
    let mut all_candidate_list = HashSet::<String>::new();
    for c in all_candidate_list_ref {
        all_candidate_list.insert(String::from(c));
    }
    log::trace!("intersection: {:?}", all_candidate_list);

    log::trace!("ssids already attempted: {:?}", ssid_attempted);
    // 2. find the complement of ssid_attempted and candidate_list
    let untried_candidate_list_ref = all_candidate_list.difference(ssid_attempted).collect::<HashSet<_>>();
    // this copy breaks the mutability issue with changing ssid_attempted after the difference is computed
    let mut untried_candidate_list = HashSet::<String>::new();
    for c in untried_candidate_list_ref {
        untried_candidate_list.insert(String::from(c));
    }
    log::trace!("untried_candidates: {:?}", untried_candidate_list);

    if untried_candidate_list.len() > 0 {
        if let Some(candidate) = untried_candidate_list.into_iter().next() {
            ssid_attempted.insert(candidate.to_string());
            log::debug!("SSID connect attempt: {:?}", candidate);
            Some(candidate.to_string())
        } else {
            log::error!("We should have had at least one item in the candidate list, but found none.");
            None
        }
    } else {
        // clear the ssid_attempted list and start from scratch
        log::debug!("Exhausted all candidates, starting over again...");
        ssid_attempted.clear();
        if let Some(candidate) = all_candidate_list.iter().next() {
            ssid_attempted.insert(candidate.to_string());
            log::debug!("SSID connect attempt: {:?}", candidate);
            Some(candidate.to_string())
        } else {
            log::info!("No SSID candidates visible");
            None
        }
    }
}