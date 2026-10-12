//! The desktop app's switches on the Connections page through the mounted UI, over a shell that is a stand-in: what each confirmation
//! says before a restart, what the shell is asked to do and nothing before the person confirms, how the answer is shown (the open
//! network's address, the refusal of a machine with none), and what is drawn for a person who may not maintain the server.
//! What is saved and restarted is `t3_desktop::exposure`'s, tested there with its own fakes; no program is run here.
use super::*;
use crate::runtime::{
    UiModelStoreExt,
    transport_tests::{change_control, click_control, control, control_attribute, input_control, rendered_text},
};
use serde_json::json;
use std::cell::{Cell, RefCell};

fn endpoint(id: &str, label: &str, url: &str, reachability: &str, status: &str) -> Endpoint {
    Endpoint::parse(&json!({"id":id,"label":label,"provider":{"id":"desktop-core"},"httpBaseUrl":url,"reachability":reachability,"status":status})).unwrap()
}
fn closed() -> ExposureView {
    ExposureView { network_accessible: false, endpoint_url: None, advertised_host: None, tailscale_serve_enabled: false, tailscale_serve_port: 443 }
}

/// A shell that records what it is asked and answers as a machine on a home network would.
struct Fake {
    local: Cell<bool>,
    view: RefCell<Option<ExposureView>>,
    endpoints: RefCell<Vec<Endpoint>>,
    calls: RefCell<Vec<String>>,
    refuse: RefCell<Option<String>>,
    /// The addresses once the network is open.
    when_open: Vec<Endpoint>,
}
impl Fake {
    fn new(view: Option<ExposureView>, endpoints: Vec<Endpoint>) -> Rc<Self> {
        let lan = endpoint("desktop-lan:http://192.168.1.20:3774", "Local network", "http://192.168.1.20:3774/", "lan", "available");
        let tailnet = endpoint("tailscale-ip:http://100.101.102.103:3774", "Tailscale IP", "http://100.101.102.103:3774/", "private-network", "available");
        Rc::new(Self { local: Cell::new(true), view: RefCell::new(view), endpoints: RefCell::new(endpoints), calls: RefCell::new(Vec::new()), refuse: RefCell::new(None), when_open: vec![lan, tailnet] })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}
impl Control for Fake {
    fn local_environment_enabled(&self) -> bool {
        self.local.get()
    }
    fn set_local_environment_enabled(&self, enabled: bool) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("local:{enabled}"));
        self.refuse.borrow().clone().map_or(Ok(()), Err)
    }
    fn exposure(&self) -> Option<ExposureView> {
        self.view.borrow().clone()
    }
    fn set_network_accessible(&self, accessible: bool) -> Result<ExposureView, String> {
        self.calls.borrow_mut().push(format!("network:{accessible}"));
        if let Some(message) = self.refuse.borrow().clone() {
            return Err(message);
        }
        let next = ExposureView { network_accessible: accessible, endpoint_url: accessible.then(|| "http://192.168.1.20:3774".to_owned()), advertised_host: accessible.then(|| "192.168.1.20".to_owned()), ..self.view.borrow().clone().unwrap() };
        *self.view.borrow_mut() = Some(next.clone());
        let mut endpoints = self.endpoints.borrow_mut();
        endpoints.retain(|endpoint| !endpoint.id.starts_with("desktop-lan:") && !endpoint.id.starts_with("tailscale-ip:"));
        if accessible {
            endpoints.extend(self.when_open.clone());
        }
        Ok(next)
    }
    fn set_tailscale_serve(&self, enabled: bool, port: u16) -> Result<ExposureView, String> {
        self.calls.borrow_mut().push(format!("serve:{enabled}:{port}"));
        if let Some(message) = self.refuse.borrow().clone() {
            return Err(message);
        }
        let next = ExposureView { tailscale_serve_enabled: enabled, tailscale_serve_port: port, ..self.view.borrow().clone().unwrap() };
        *self.view.borrow_mut() = Some(next.clone());
        for endpoint in self.endpoints.borrow_mut().iter_mut().filter(|endpoint| endpoint.id.starts_with("tailscale-magicdns:")) {
            endpoint.available = enabled;
        }
        Ok(next)
    }
    fn endpoints(&self) -> LocalBoxFuture<'static, Vec<Endpoint>> {
        Box::pin(std::future::ready(self.endpoints.borrow().clone()))
    }
}

#[derive(Clone)]
struct Props {
    fake: Rc<Fake>,
    can_manage: bool,
}
impl PartialEq for Props {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.fake, &other.fake) && self.can_manage == other.can_manage
    }
}
fn harness(props: Props) -> Element {
    let state = use_store(UiModel::default);
    rsx! { Rows { state, can_manage: props.can_manage, control: Handle(props.fake) } }
}
fn mount(fake: &Rc<Fake>, can_manage: bool) -> VirtualDom {
    let mut dom = VirtualDom::new_with_props(harness, Props { fake: fake.clone(), can_manage });
    dom.rebuild_in_place();
    settle(&mut dom);
    dom
}
/// Runs the work the page has ready (the addresses being read, a confirmed change) until it has none, and draws it.
fn settle(dom: &mut VirtualDom) {
    use futures_util::FutureExt;
    for _ in 0..64 {
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        if dom.wait_for_work().now_or_never().is_none() {
            break;
        }
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}
fn checked(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "checked").and_then(|(_, value)| value).as_deref() == Some("true")
}

#[tokio::test(flavor = "current_thread")]
async fn opening_the_network_asks_first_then_restarts_and_names_the_address_a_device_can_reach() {
    let fake = Fake::new(Some(closed()), vec![endpoint("desktop-loopback:3774", "This machine", "http://127.0.0.1:3774/", "loopback", "available")]);
    let mut dom = mount(&fake, true);
    assert!(rendered_text(&dom).contains("Limited to this machine."));
    assert!(!checked(&dom, "Enable network access"));

    change_control(&mut dom, "Enable network access", "true");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Enable network access?") && rendered_text(&dom).contains("T3 Code will restart."));
    assert!(fake.calls().is_empty(), "nothing is asked until the person confirms");
    click_control(&mut dom, "Cancel network access");
    settle(&mut dom);
    assert!(control(&dom, "Restart and enable").is_none() && fake.calls().is_empty());

    change_control(&mut dom, "Enable network access", "true");
    settle(&mut dom);
    click_control(&mut dom, "Restart and enable");
    settle(&mut dom);
    assert_eq!(fake.calls(), ["network:true"]);
    assert!(control(&dom, "Restart and enable").is_none(), "the question is answered");
    assert!(checked(&dom, "Enable network access"));
    let text = rendered_text(&dom);
    assert!(text.contains("Reachable at http://192.168.1.20:3774/") && text.contains("+2"), "the default address is named and the other is a click away: {text}");

    // Closing it again is asked, too, and says what keeps working.
    change_control(&mut dom, "Enable network access", "false");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Disable network access?") && rendered_text(&dom).contains("Existing tunnels"));
    click_control(&mut dom, "Restart and disable");
    settle(&mut dom);
    assert_eq!(fake.calls(), ["network:true", "network:false"]);
    assert!(rendered_text(&dom).contains("Limited to this machine."));
}

#[tokio::test(flavor = "current_thread")]
async fn the_other_addresses_are_listed_on_request_and_one_can_be_made_the_default() {
    let mut open = closed();
    open.network_accessible = true;
    open.endpoint_url = Some("http://192.168.1.20:3774".into());
    let fake = Fake::new(Some(open), vec![
        endpoint("desktop-loopback:3774", "This machine", "http://127.0.0.1:3774/", "loopback", "available"),
        endpoint("desktop-lan:http://192.168.1.20:3774", "Local network", "http://192.168.1.20:3774/", "lan", "available"),
        endpoint("tailscale-ip:http://100.101.102.103:3774", "Tailscale IP", "http://100.101.102.103:3774/", "private-network", "available"),
        endpoint("tailscale-magicdns:https://box.tail1234.ts.net/", "Tailscale HTTPS", "https://box.tail1234.ts.net/", "private-network", "unavailable"),
    ]);
    let mut dom = mount(&fake, true);
    assert!(control(&dom, "Address This machine").is_none(), "closed until asked for");
    click_control(&mut dom, "Show the other addresses");
    settle(&mut dom);
    for label in ["This machine", "Local network", "Tailscale IP"] {
        assert!(control(&dom, &format!("Address {label}")).is_some(), "{label}");
    }
    assert!(control(&dom, "Address Tailscale HTTPS").is_none(), "the HTTPS address has its own row");
    assert!(control(&dom, "Set Local network as default").is_none(), "it is the default already");
    click_control(&mut dom, "Set Tailscale IP as default");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Reachable at http://100.101.102.103:3774/"), "{}", rendered_text(&dom));
    assert!(control(&dom, "Set Tailscale IP as default").is_none() && control(&dom, "Set Local network as default").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn a_machine_with_no_address_to_be_reached_at_is_refused_in_the_shells_words_and_stays_closed() {
    let fake = Fake::new(Some(closed()), vec![]);
    *fake.refuse.borrow_mut() = Some("No reachable network address is available for desktop network access on port 3774.".into());
    let mut dom = mount(&fake, true);
    change_control(&mut dom, "Enable network access", "true");
    settle(&mut dom);
    click_control(&mut dom, "Restart and enable");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("No reachable network address is available for desktop network access on port 3774."));
    assert!(!checked(&dom, "Enable network access") && control(&dom, "Restart and enable").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn tailscale_https_is_set_up_on_a_port_the_person_gives_and_turned_off_again() {
    let off = endpoint("tailscale-magicdns:https://box.tail1234.ts.net/", "Tailscale HTTPS", "https://box.tail1234.ts.net/", "private-network", "unavailable");
    let fake = Fake::new(Some(ExposureView { network_accessible: true, ..closed() }), vec![off]);
    let mut dom = mount(&fake, true);
    assert!(rendered_text(&dom).contains("Use Tailscale Serve to expose this backend through a MagicDNS HTTPS URL."));
    assert!(!checked(&dom, "Enable Tailscale HTTPS"));

    change_control(&mut dom, "Enable Tailscale HTTPS", "true");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Set up Tailscale HTTPS?") && rendered_text(&dom).contains("https://box.tail1234.ts.net/"));
    input_control(&mut dom, "HTTPS port", "70000");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Enter a port from 1 to 65535."));
    assert_eq!(control_attribute(&dom, "Enable", "disabled").unwrap().1.as_deref(), Some("true"));
    input_control(&mut dom, "HTTPS port", "8443");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("https://box.tail1234.ts.net:8443/"), "the address the port gives is shown before it is asked for");
    click_control(&mut dom, "Enable");
    settle(&mut dom);
    assert_eq!(fake.calls(), ["serve:true:8443"]);
    assert!(checked(&dom, "Enable Tailscale HTTPS"));

    change_control(&mut dom, "Enable Tailscale HTTPS", "false");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Disable Tailscale HTTPS?"));
    click_control(&mut dom, "Restart and disable");
    settle(&mut dom);
    assert_eq!(fake.calls(), ["serve:true:8443", "serve:false:8443"], "the port it was set up on is kept");
    assert!(!checked(&dom, "Enable Tailscale HTTPS"));
}

#[tokio::test(flavor = "current_thread")]
async fn without_tailscale_running_the_row_says_how_to_start_and_offers_no_switch() {
    let fake = Fake::new(Some(closed()), vec![]);
    let dom = mount(&fake, true);
    assert!(rendered_text(&dom).contains("Start Tailscale to set up HTTPS access through MagicDNS."));
    assert!(control(&dom, "Enable Tailscale HTTPS").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn the_local_environment_switch_asks_then_restarts_the_app_and_a_refusal_is_said() {
    let fake = Fake::new(Some(closed()), vec![]);
    let mut dom = mount(&fake, true);
    assert!(checked(&dom, "Local environment") && rendered_text(&dom).contains("Run agents on this computer."));
    change_control(&mut dom, "Local environment", "false");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Turn off local environment?") && rendered_text(&dom).contains("Your projects, history, and remote environments are unaffected."));
    assert!(fake.calls().is_empty());
    *fake.refuse.borrow_mut() = Some("Could not save the setting. disk full".into());
    click_control(&mut dom, "Restart and turn off");
    settle(&mut dom);
    assert_eq!(fake.calls(), ["local:false"]);
    assert!(rendered_text(&dom).contains("Could not save the setting. disk full") || control(&dom, "Restart and turn off").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn a_turned_off_local_environment_can_be_turned_on_and_has_no_network_rows() {
    let fake = Fake::new(None, vec![]);
    fake.local.set(false);
    let mut dom = mount(&fake, true);
    let text = rendered_text(&dom);
    assert!(text.contains("Turned off. Agents only run in remote environments."), "{text}");
    assert!(control(&dom, "Enable network access").is_none() && !text.contains("Network access"), "there is no server of this app's to open");
    change_control(&mut dom, "Local environment", "true");
    settle(&mut dom);
    assert!(rendered_text(&dom).contains("Turn on local environment?"));
    click_control(&mut dom, "Restart and turn on");
    settle(&mut dom);
    assert_eq!(fake.calls(), ["local:true"]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_person_who_may_not_maintain_the_server_sees_the_local_environment_only() {
    let fake = Fake::new(Some(closed()), vec![]);
    let dom = mount(&fake, false);
    assert!(control(&dom, "Local environment").is_some());
    assert!(control(&dom, "Enable network access").is_none() && control(&dom, "Enable Tailscale HTTPS").is_none());
}

#[test]
fn the_port_is_digits_from_one_to_65535_and_the_address_carries_it_unless_it_is_443() {
    assert_eq!([parse_port("8443"), parse_port(" 1 "), parse_port("65535")], [Some(8443), Some(1), Some(65535)]);
    assert_eq!([parse_port(""), parse_port("0"), parse_port("65536"), parse_port("-5"), parse_port("44a"), parse_port("4.5")], [None; 6]);
    assert_eq!(serve_url("https://box.tail1234.ts.net/", Some(8443)), "https://box.tail1234.ts.net:8443/");
    assert_eq!(serve_url("https://box.tail1234.ts.net/", Some(443)), "https://box.tail1234.ts.net/");
    assert_eq!(serve_url("https://box.tail1234.ts.net/", None), "https://box.tail1234.ts.net/");
}
