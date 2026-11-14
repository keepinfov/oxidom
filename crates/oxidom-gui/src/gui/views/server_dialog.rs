//! The dialog a server is typed into, field by field.
//!
//! Row titles are the draft's own JSON keys, deliberately: the stored server,
//! the CLI template and this dialog must read as one thing, and a prettier
//! label here would be a third name for the same field. The pure half —
//! [`DialogValues`] → [`draft_from_values`] → [`values_issue`] — is what the
//! tests exercise; validation is `oxidom_core::draft::resolve`, the same
//! validator the daemon runs, so the dialog and the daemon reject with one
//! sentence.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use oxidom_core::draft::ServerDraft;
use oxidom_core::model::{
    Hysteria2Obfs, Hysteria2Settings, Protocol, Server, StreamSettings, parse_bandwidth_mbps,
};

use super::{set_transient_parent, set_validation, validation_label};

pub struct ServerDialogCallbacks {
    pub create: Rc<dyn Fn(ServerDraft)>,
}

/// The protocols the dialog offers, in combo order. Socks and http exist in
/// the model but are plain proxies with no fields of their own; the CLI
/// template covers them, and offering them here would double the combo for
/// two rows nobody types.
const PROTOCOLS: [(Protocol, &str); 5] = [
    (Protocol::Vless, "vless"),
    (Protocol::Vmess, "vmess"),
    (Protocol::Trojan, "trojan"),
    (Protocol::Shadowsocks, "shadowsocks"),
    (Protocol::Hysteria2, "hysteria2"),
];

const NETWORKS: [&str; 8] = [
    "tcp",
    "ws",
    "grpc",
    "xhttp",
    "splithttp",
    "h2",
    "httpupgrade",
    "kcp",
];
const SECURITIES: [&str; 3] = ["none", "tls", "reality"];

/// Everything the widgets hold, as plain data, so assembling and validating a
/// draft needs no widget and no display.
#[derive(Debug, Clone, Default)]
pub struct DialogValues {
    pub name: String,
    pub protocol_index: usize,
    pub address: String,
    pub port: u16,
    pub uuid: String,
    pub alter_id: u32,
    pub vmess_security: String,
    pub method: String,
    pub password: String,
    pub auth: String,
    pub network_index: usize,
    pub security_index: usize,
    pub sni: String,
    pub alpn: String,
    pub fingerprint: String,
    pub path: String,
    pub host: String,
    pub service_name: String,
    pub header_type: String,
    pub flow: String,
    pub public_key: String,
    pub short_id: String,
    pub spider_x: String,
    pub h2_sni: String,
    pub h2_alpn: String,
    pub h2_obfs_password: String,
    pub h2_up: String,
    pub h2_down: String,
    pub patch: String,
    pub base_draft: Option<ServerDraft>,
}

impl DialogValues {
    fn protocol(&self) -> Protocol {
        PROTOCOLS
            .get(self.protocol_index)
            .map(|(protocol, _)| *protocol)
            .unwrap_or(Protocol::Vless)
    }
}

fn opt(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn alpn_list(value: &str) -> Option<Vec<String>> {
    let list: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    (!list.is_empty()).then_some(list)
}

fn combo_index(options: &[&str], value: &str) -> usize {
    options
        .iter()
        .position(|option| *option == value)
        .unwrap_or(0)
}

/// A stored server as the dialog shows it — the prefill for editing. The
/// inverse of [`draft_from_values`]: an absent optional is an empty row,
/// not a placeholder.
pub fn values_from_server(server: &Server) -> DialogValues {
    let draft = oxidom_core::draft::draft_from_server(server);
    let base_draft = Some(draft.clone());
    let stream = draft.stream.unwrap_or_default();
    let hysteria2 = draft.hysteria2.unwrap_or_default();
    let option = |value: &Option<String>| value.clone().unwrap_or_default();
    DialogValues {
        name: draft.name,
        protocol_index: combo_index(
            &PROTOCOLS
                .iter()
                .map(|(_, label)| *label)
                .collect::<Vec<_>>(),
            server.protocol.as_str(),
        ),
        address: draft.address,
        port: draft.port,
        uuid: option(&draft.uuid),
        alter_id: draft.alter_id.unwrap_or(0),
        vmess_security: option(&draft.security),
        method: option(&draft.method),
        password: option(&draft.password),
        auth: option(&draft.auth),
        network_index: combo_index(&NETWORKS, &stream.network),
        security_index: combo_index(&SECURITIES, &stream.security),
        sni: option(&stream.sni),
        alpn: stream
            .alpn
            .clone()
            .map(|list| list.join(", "))
            .unwrap_or_default(),
        fingerprint: option(&stream.fingerprint),
        path: option(&stream.path),
        host: option(&stream.host),
        service_name: option(&stream.service_name),
        header_type: option(&stream.header_type),
        flow: option(&stream.flow),
        public_key: option(&stream.public_key),
        short_id: option(&stream.short_id),
        spider_x: option(&stream.spider_x),
        h2_sni: option(&hysteria2.sni),
        h2_alpn: hysteria2
            .alpn
            .clone()
            .map(|list| list.join(", "))
            .unwrap_or_default(),
        h2_obfs_password: hysteria2.obfs.map(|obfs| obfs.password).unwrap_or_default(),
        h2_up: hysteria2
            .up_mbps
            .map(|up| up.to_string())
            .unwrap_or_default(),
        h2_down: hysteria2
            .down_mbps
            .map(|down| down.to_string())
            .unwrap_or_default(),
        patch: draft
            .outbound_patch
            .as_ref()
            .map(|patch| serde_json::to_string_pretty(patch).unwrap_or_default())
            .unwrap_or_default(),
        base_draft,
    }
}

/// The widgets' state as a draft — or the sentence that stops it, before the
/// daemon is ever asked.
pub fn draft_from_values(values: &DialogValues) -> Result<ServerDraft, String> {
    let patch = match values.patch.trim() {
        "" => None,
        text => Some(
            serde_json::from_str::<serde_json::Value>(text)
                .map_err(|error| format!("outbound_patch does not parse as JSON: {error}"))?,
        ),
    };
    let protocol = values.protocol();
    let base_stream = values
        .base_draft
        .as_ref()
        .and_then(|draft| draft.stream.clone())
        .unwrap_or_default();
    let base_hysteria2 = values
        .base_draft
        .as_ref()
        .and_then(|draft| draft.hysteria2.clone())
        .unwrap_or_default();
    let obfs_kind = base_hysteria2
        .obfs
        .as_ref()
        .map(|obfs| obfs.kind.clone())
        .unwrap_or_else(|| "salamander".to_string());
    let stream = matches!(
        protocol,
        Protocol::Vless | Protocol::Vmess | Protocol::Trojan
    )
    .then(|| StreamSettings {
        network: NETWORKS
            .get(values.network_index)
            .copied()
            .unwrap_or("tcp")
            .to_string(),
        security: SECURITIES
            .get(values.security_index)
            .copied()
            .unwrap_or("none")
            .to_string(),
        sni: opt(&values.sni),
        alpn: alpn_list(&values.alpn),
        fingerprint: opt(&values.fingerprint),
        public_key: opt(&values.public_key),
        short_id: opt(&values.short_id),
        spider_x: opt(&values.spider_x),
        path: opt(&values.path),
        host: opt(&values.host),
        service_name: opt(&values.service_name),
        header_type: opt(&values.header_type),
        flow: opt(&values.flow),
        ..base_stream
    });
    let hysteria2 = matches!(protocol, Protocol::Hysteria2).then(|| Hysteria2Settings {
        sni: opt(&values.h2_sni),
        alpn: alpn_list(&values.h2_alpn),
        obfs: opt(&values.h2_obfs_password).map(|password| Hysteria2Obfs {
            kind: obfs_kind,
            password,
        }),
        up_mbps: opt(&values.h2_up).and_then(|raw| parse_bandwidth_mbps(&raw)),
        down_mbps: opt(&values.h2_down).and_then(|raw| parse_bandwidth_mbps(&raw)),
        ..base_hysteria2
    });
    Ok(ServerDraft {
        name: values.name.trim().to_string(),
        protocol,
        address: values.address.trim().to_string(),
        port: values.port,
        uuid: opt(&values.uuid),
        encryption: values
            .base_draft
            .as_ref()
            .and_then(|draft| draft.encryption.clone()),
        alter_id: Some(values.alter_id),
        security: opt(&values.vmess_security),
        method: opt(&values.method),
        password: opt(&values.password),
        username: values
            .base_draft
            .as_ref()
            .and_then(|draft| draft.username.clone()),
        auth: opt(&values.auth),
        stream,
        hysteria2,
        outbound_patch: patch,
    })
}

/// What stops these values from becoming a server, in the daemon's own words.
pub fn values_issue(values: &DialogValues) -> Option<String> {
    match draft_from_values(values) {
        Err(sentence) => Some(sentence),
        Ok(draft) => oxidom_core::draft::resolve(&draft)
            .err()
            .map(|error| error.to_string()),
    }
}

/// Which field key a validator sentence names, so the offending row can carry
/// the error state alongside the top banner.
pub fn invalid_field(issue: Option<&str>) -> Option<&'static str> {
    match issue?.split_whitespace().next()? {
        "address" => Some("address"),
        "port" => Some("port"),
        "uuid" => Some("uuid"),
        "password" => Some("password"),
        "method" => Some("method"),
        "auth" => Some("auth"),
        "public_key" => Some("public_key"),
        "short_id" => Some("short_id"),
        "sni" => Some("sni"),
        "up_mbps" => Some("up_mbps"),
        "down_mbps" => Some("down_mbps"),
        "outbound_patch" => Some("outbound_patch"),
        _ => None,
    }
}

/// Whether a transport/TLS detail row belongs on screen for `(protocol, network,
/// security)`. Any row that already holds a value stays visible regardless, so
/// editing a stored server never hides a populated field behind another
/// transport choice.
pub fn stream_field_visible(
    field: &str,
    protocol: Protocol,
    network: &str,
    security: &str,
    current_value: &str,
) -> bool {
    if !current_value.trim().is_empty() {
        return true;
    }
    match field {
        "sni" | "fingerprint" => matches!(security, "tls" | "reality"),
        "alpn" => security == "tls",
        "public_key" | "short_id" | "spider_x" => security == "reality",
        "flow" => protocol == Protocol::Vless && matches!(security, "tls" | "reality"),
        "path" => matches!(
            network,
            "ws" | "xhttp" | "splithttp" | "httpupgrade" | "h2" | "tcp" | "kcp"
        ),
        "host" => matches!(
            network,
            "ws" | "xhttp" | "splithttp" | "httpupgrade" | "h2" | "tcp"
        ),
        "service_name" => network == "grpc",
        "header_type" => matches!(network, "tcp" | "kcp"),
        _ => true,
    }
}

fn mark_field_error(widget: &impl IsA<gtk::Widget>, active: bool, issue: Option<&str>) {
    let widget = widget.upcast_ref::<gtk::Widget>();
    if active {
        widget.add_css_class("error");
        widget.update_state(&[gtk::accessible::State::Invalid(
            gtk::AccessibleInvalidState::True,
        )]);
        if let Some(issue) = issue {
            widget.update_property(&[gtk::accessible::Property::Description(issue)]);
        }
    } else {
        widget.remove_css_class("error");
        widget.update_state(&[gtk::accessible::State::Invalid(
            gtk::AccessibleInvalidState::False,
        )]);
        widget.update_property(&[gtk::accessible::Property::Description("")]);
    }
}

pub fn show_server_dialog(parent: &impl IsA<gtk::Widget>, callbacks: ServerDialogCallbacks) {
    build_dialog(
        parent,
        "Create server",
        "Create",
        None,
        false,
        callbacks.create,
    );
}

/// The same rows, prefilled from a stored server. `lock_protocol` is set
/// for a subscription's server, whose protocol is the provider's own.
pub fn show_server_edit_dialog(
    parent: &impl IsA<gtk::Widget>,
    prefill: DialogValues,
    lock_protocol: bool,
    on_save: Rc<dyn Fn(ServerDraft)>,
) {
    build_dialog(
        parent,
        "Edit server",
        "Save",
        Some(Box::new(prefill)),
        lock_protocol,
        on_save,
    );
}

fn build_dialog(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    button_label: &str,
    prefill: Option<Box<DialogValues>>,
    lock_protocol: bool,
    on_confirm: Rc<dyn Fn(ServerDraft)>,
) {
    let window = adw::Window::builder()
        .title(title)
        .modal(true)
        .default_width(520)
        .default_height(680)
        .build();
    set_transient_parent(&window, parent);

    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let create = gtk::Button::with_label(button_label);
    create.add_css_class("suggested-action");
    create.set_sensitive(false);
    header.pack_start(&cancel);
    header.pack_end(&create);

    let entry = |title: &str| {
        adw::EntryRow::builder()
            .title(title)
            .activates_default(true)
            .build()
    };
    let secret = |title: &str| adw::PasswordEntryRow::builder().title(title).build();
    let combo = |title: &str, options: &[&str]| {
        adw::ComboRow::builder()
            .title(title)
            .model(&gtk::StringList::new(options))
            .build()
    };

    let server_group = adw::PreferencesGroup::builder()
        .title("Server")
        .description("Row titles are the JSON keys the daemon stores.")
        .build();
    let name = entry("name");
    let protocol_labels: Vec<&str> = PROTOCOLS.iter().map(|(_, label)| *label).collect();
    let protocol = combo("protocol", &protocol_labels);
    let address = entry("address");
    let port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    port.set_title("port");
    port.set_value(443.0);
    server_group.add(&name);
    server_group.add(&protocol);
    server_group.add(&address);
    server_group.add(&port);

    let credentials_group = adw::PreferencesGroup::builder()
        .title("Credentials")
        .build();
    let uuid = secret("uuid");
    let alter_id = adw::SpinRow::with_range(0.0, 65535.0, 1.0);
    alter_id.set_title("alter_id");
    let vmess_security = entry("security");
    let method = entry("method");
    let password = secret("password");
    let auth = secret("auth");
    credentials_group.add(&uuid);
    credentials_group.add(&alter_id);
    credentials_group.add(&vmess_security);
    credentials_group.add(&method);
    credentials_group.add(&password);
    credentials_group.add(&auth);

    let stream_group = adw::PreferencesGroup::builder()
        .title("Transport and TLS")
        .build();
    let network = combo("network", &NETWORKS);
    let security = combo("security", &SECURITIES);
    let sni = entry("sni");
    let alpn = entry("alpn");
    let fingerprint = entry("fingerprint");
    let path = entry("path");
    let host = entry("host");
    let service_name = entry("service_name");
    let header_type = entry("header_type");
    let flow = entry("flow");
    let public_key = entry("public_key");
    let short_id = entry("short_id");
    let spider_x = entry("spider_x");
    stream_group.add(&network);
    stream_group.add(&security);
    for row in [
        &sni,
        &alpn,
        &fingerprint,
        &path,
        &host,
        &service_name,
        &header_type,
        &flow,
        &public_key,
        &short_id,
        &spider_x,
    ] {
        stream_group.add(row);
    }

    let hysteria2_group = adw::PreferencesGroup::builder().title("hysteria2").build();
    let h2_sni = entry("sni");
    let h2_alpn = entry("alpn");
    let h2_obfs_password = secret("obfs.password");
    let h2_up = entry("up_mbps");
    let h2_down = entry("down_mbps");
    hysteria2_group.add(&h2_sni);
    hysteria2_group.add(&h2_alpn);
    hysteria2_group.add(&h2_obfs_password);
    hysteria2_group.add(&h2_up);
    hysteria2_group.add(&h2_down);

    let patch_group = adw::PreferencesGroup::builder()
        .title("outbound_patch")
        .description(
            "Raw JSON merged into the generated outbound (RFC 7396), for a core option \
             the fields above do not model. It may not set \"tag\" or \"protocol\".",
        )
        .build();
    let patch_buffer = gtk::TextBuffer::new(None);
    let patch_editor = gtk::TextView::builder()
        .buffer(&patch_buffer)
        .monospace(true)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(12)
        .right_margin(12)
        .wrap_mode(gtk::WrapMode::Char)
        .build();
    patch_editor.update_property(&[gtk::accessible::Property::Label("Outbound patch JSON")]);
    let patch_scroller = gtk::ScrolledWindow::builder()
        .min_content_height(80)
        .max_content_height(160)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&patch_editor)
        .build();
    let patch_frame = gtk::Frame::builder()
        .child(&patch_scroller)
        .css_classes(["card"])
        .build();
    patch_group.add(&patch_frame);

    let validation = validation_label();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.append(&validation);
    for group in [
        &server_group,
        &credentials_group,
        &stream_group,
        &hysteria2_group,
        &patch_group,
    ] {
        content.append(group);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&scroller);
    window.set_content(Some(&page));
    window.set_default_widget(Some(&create));

    let values = Rc::new(RefCell::new(DialogValues {
        port: 443,
        ..DialogValues::default()
    }));

    let base_draft_for_collect = prefill.as_ref().and_then(|p| p.base_draft.clone());
    if let Some(prefill) = prefill {
        // The rows carry the server as it stands, not a blank form: the
        // protocol is a fact about the server being edited, and for a
        // subscription's server it is the provider's fact, not a choice.
        name.set_text(&prefill.name);
        protocol.set_selected(
            PROTOCOLS
                .iter()
                .position(|(candidate, _)| *candidate == prefill.protocol())
                .unwrap_or(0) as u32,
        );
        protocol.set_sensitive(!lock_protocol);
        address.set_text(&prefill.address);
        port.set_value(f64::from(prefill.port));
        uuid.set_text(&prefill.uuid);
        alter_id.set_value(f64::from(prefill.alter_id));
        vmess_security.set_text(&prefill.vmess_security);
        method.set_text(&prefill.method);
        password.set_text(&prefill.password);
        auth.set_text(&prefill.auth);
        network.set_selected(prefill.network_index as u32);
        security.set_selected(prefill.security_index as u32);
        sni.set_text(&prefill.sni);
        alpn.set_text(&prefill.alpn);
        fingerprint.set_text(&prefill.fingerprint);
        path.set_text(&prefill.path);
        host.set_text(&prefill.host);
        service_name.set_text(&prefill.service_name);
        header_type.set_text(&prefill.header_type);
        flow.set_text(&prefill.flow);
        public_key.set_text(&prefill.public_key);
        short_id.set_text(&prefill.short_id);
        spider_x.set_text(&prefill.spider_x);
        h2_sni.set_text(&prefill.h2_sni);
        h2_alpn.set_text(&prefill.h2_alpn);
        h2_obfs_password.set_text(&prefill.h2_obfs_password);
        h2_up.set_text(&prefill.h2_up);
        h2_down.set_text(&prefill.h2_down);
        if !prefill.patch.is_empty() {
            patch_buffer.set_text(&prefill.patch);
        }
    }

    let collect = {
        let values = values.clone();
        let widgets = (
            name.clone(),
            protocol.clone(),
            address.clone(),
            port.clone(),
            (
                uuid.clone(),
                alter_id.clone(),
                vmess_security.clone(),
                method.clone(),
                password.clone(),
                auth.clone(),
            ),
            (
                network.clone(),
                security.clone(),
                sni.clone(),
                alpn.clone(),
                fingerprint.clone(),
                path.clone(),
                host.clone(),
                service_name.clone(),
                header_type.clone(),
                flow.clone(),
                public_key.clone(),
                short_id.clone(),
                spider_x.clone(),
            ),
            (
                h2_sni.clone(),
                h2_alpn.clone(),
                h2_obfs_password.clone(),
                h2_up.clone(),
                h2_down.clone(),
            ),
            patch_buffer.clone(),
        );
        move || {
            let (name, protocol, address, port, credentials, stream, hysteria2, patch_buffer) =
                &widgets;
            let (uuid, alter_id, vmess_security, method, password, auth) = credentials;
            let (
                network,
                security,
                sni,
                alpn,
                fingerprint,
                path,
                host,
                service_name,
                header_type,
                flow,
                public_key,
                short_id,
                spider_x,
            ) = stream;
            let (h2_sni, h2_alpn, h2_obfs_password, h2_up, h2_down) = hysteria2;
            let (start, end) = patch_buffer.bounds();
            let collected = DialogValues {
                name: name.text().to_string(),
                protocol_index: protocol.selected() as usize,
                address: address.text().to_string(),
                port: port.value() as u16,
                uuid: uuid.text().to_string(),
                alter_id: alter_id.value() as u32,
                vmess_security: vmess_security.text().to_string(),
                method: method.text().to_string(),
                password: password.text().to_string(),
                auth: auth.text().to_string(),
                network_index: network.selected() as usize,
                security_index: security.selected() as usize,
                sni: sni.text().to_string(),
                alpn: alpn.text().to_string(),
                fingerprint: fingerprint.text().to_string(),
                path: path.text().to_string(),
                host: host.text().to_string(),
                service_name: service_name.text().to_string(),
                header_type: header_type.text().to_string(),
                flow: flow.text().to_string(),
                public_key: public_key.text().to_string(),
                short_id: short_id.text().to_string(),
                spider_x: spider_x.text().to_string(),
                h2_sni: h2_sni.text().to_string(),
                h2_alpn: h2_alpn.text().to_string(),
                h2_obfs_password: h2_obfs_password.text().to_string(),
                h2_up: h2_up.text().to_string(),
                h2_down: h2_down.text().to_string(),
                patch: patch_buffer.text(&start, &end, false).to_string(),
                base_draft: base_draft_for_collect.clone(),
            };
            *values.borrow_mut() = collected;
        }
    };

    let refresh = {
        let values = values.clone();
        let collect = collect.clone();
        let create = create.clone();
        let validation = validation.clone();
        let address = address.clone();
        let port = port.clone();
        let credentials_rows = (
            uuid.clone(),
            alter_id.clone(),
            vmess_security.clone(),
            method.clone(),
            password.clone(),
            auth.clone(),
        );
        let stream_rows = (
            sni.clone(),
            alpn.clone(),
            fingerprint.clone(),
            path.clone(),
            host.clone(),
            service_name.clone(),
            header_type.clone(),
            flow.clone(),
            public_key.clone(),
            short_id.clone(),
            spider_x.clone(),
        );
        let h2_rows = (h2_sni.clone(), h2_up.clone(), h2_down.clone());
        let patch_frame = patch_frame.clone();
        let stream_group = stream_group.clone();
        let hysteria2_group = hysteria2_group.clone();
        Rc::new(move || {
            collect();
            let values = values.borrow();
            let protocol = values.protocol();
            let (uuid, alter_id, vmess_security, method, password, auth) = &credentials_rows;
            let (
                sni,
                alpn,
                fingerprint,
                path,
                host,
                service_name,
                header_type,
                flow,
                public_key,
                short_id,
                spider_x,
            ) = &stream_rows;
            let (h2_sni, h2_up, h2_down) = &h2_rows;
            uuid.set_visible(matches!(protocol, Protocol::Vless | Protocol::Vmess));
            alter_id.set_visible(matches!(protocol, Protocol::Vmess));
            vmess_security.set_visible(matches!(protocol, Protocol::Vmess));
            method.set_visible(matches!(protocol, Protocol::Shadowsocks));
            password.set_visible(matches!(protocol, Protocol::Trojan | Protocol::Shadowsocks));
            auth.set_visible(matches!(protocol, Protocol::Hysteria2));
            stream_group.set_visible(matches!(
                protocol,
                Protocol::Vless | Protocol::Vmess | Protocol::Trojan
            ));
            hysteria2_group.set_visible(matches!(protocol, Protocol::Hysteria2));

            let net = NETWORKS.get(values.network_index).copied().unwrap_or("tcp");
            let sec = SECURITIES
                .get(values.security_index)
                .copied()
                .unwrap_or("none");
            for (field, row, current) in [
                ("sni", sni, values.sni.as_str()),
                ("alpn", alpn, values.alpn.as_str()),
                ("fingerprint", fingerprint, values.fingerprint.as_str()),
                ("path", path, values.path.as_str()),
                ("host", host, values.host.as_str()),
                ("service_name", service_name, values.service_name.as_str()),
                ("header_type", header_type, values.header_type.as_str()),
                ("flow", flow, values.flow.as_str()),
                ("public_key", public_key, values.public_key.as_str()),
                ("short_id", short_id, values.short_id.as_str()),
                ("spider_x", spider_x, values.spider_x.as_str()),
            ] {
                row.set_visible(stream_field_visible(field, protocol, net, sec, current));
            }

            let issue = values_issue(&values);
            create.set_sensitive(issue.is_none());
            // An untouched dialog is incomplete, not wrong: the sentence would
            // open red on "address is empty" before anything was typed.
            let touched = !values.address.trim().is_empty()
                || !values.uuid.trim().is_empty()
                || !values.password.trim().is_empty()
                || !values.method.trim().is_empty()
                || !values.auth.trim().is_empty()
                || !values.patch.trim().is_empty();
            let visible_issue = issue.as_deref().filter(|_| touched);
            set_validation(&validation, visible_issue);
            let bad_field = invalid_field(visible_issue);
            mark_field_error(&address, bad_field == Some("address"), visible_issue);
            mark_field_error(&port, bad_field == Some("port"), visible_issue);
            mark_field_error(uuid, bad_field == Some("uuid"), visible_issue);
            mark_field_error(password, bad_field == Some("password"), visible_issue);
            mark_field_error(method, bad_field == Some("method"), visible_issue);
            mark_field_error(auth, bad_field == Some("auth"), visible_issue);
            mark_field_error(public_key, bad_field == Some("public_key"), visible_issue);
            mark_field_error(short_id, bad_field == Some("short_id"), visible_issue);
            mark_field_error(
                sni,
                bad_field == Some("sni") && protocol != Protocol::Hysteria2,
                visible_issue,
            );
            mark_field_error(
                h2_sni,
                bad_field == Some("sni") && protocol == Protocol::Hysteria2,
                visible_issue,
            );
            mark_field_error(h2_up, bad_field == Some("up_mbps"), visible_issue);
            mark_field_error(h2_down, bad_field == Some("down_mbps"), visible_issue);
            mark_field_error(
                &patch_frame,
                bad_field == Some("outbound_patch"),
                visible_issue,
            );
        })
    };
    refresh();

    for row in [
        &name,
        &address,
        &vmess_security,
        &method,
        &sni,
        &alpn,
        &fingerprint,
        &path,
        &host,
        &service_name,
        &header_type,
        &flow,
        &public_key,
        &short_id,
        &spider_x,
        &h2_sni,
        &h2_alpn,
        &h2_up,
        &h2_down,
    ] {
        let refresh = refresh.clone();
        row.connect_changed(move |_| refresh());
    }
    for row in [&uuid, &password, &auth, &h2_obfs_password] {
        let refresh = refresh.clone();
        row.connect_changed(move |_| refresh());
    }
    for row in [&port, &alter_id] {
        let refresh = refresh.clone();
        row.connect_changed(move |_| refresh());
    }
    for row in [&protocol, &network, &security] {
        let refresh = refresh.clone();
        row.connect_selected_notify(move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        patch_buffer.connect_changed(move |_| refresh());
    }

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());

    let window_for_create = window.clone();
    create.connect_clicked(move |_| {
        collect();
        let Ok(draft) = draft_from_values(&values.borrow()) else {
            return;
        };
        if oxidom_core::draft::resolve(&draft).is_err() {
            return;
        }
        on_confirm(draft);
        window_for_create.close();
    });

    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vless_values() -> DialogValues {
        DialogValues {
            name: "Typed".to_string(),
            protocol_index: 0,
            address: "server.example.invalid".to_string(),
            port: 443,
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            network_index: 1,  // ws
            security_index: 2, // reality
            sni: "cover.example.invalid".to_string(),
            public_key: "invented-pbk".to_string(),
            short_id: "0123ab".to_string(),
            ..DialogValues::default()
        }
    }

    /// What the dialog displays is what the draft carries: same keys, same
    /// values, empty rows absent rather than empty strings.
    #[test]
    fn the_dialog_assembles_the_draft_it_displays() {
        let draft = draft_from_values(&vless_values()).expect("assembles");
        assert_eq!(draft.protocol, Protocol::Vless);
        assert_eq!(draft.address, "server.example.invalid");
        assert_eq!(
            draft.uuid.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        let stream = draft.stream.expect("a vless draft carries a stream block");
        assert_eq!(stream.network, "ws");
        assert_eq!(stream.security, "reality");
        assert_eq!(stream.public_key.as_deref(), Some("invented-pbk"));
        assert_eq!(stream.path, None, "an empty row is absent, not empty");
        assert!(draft.hysteria2.is_none(), "no hysteria2 block for vless");
        assert!(values_issue(&vless_values()).is_none());
    }

    /// The rejection sentence is the daemon's own: `values_issue` runs the
    /// same `draft::resolve` the daemon runs, so the dialog cannot drift into
    /// wording of its own.
    #[test]
    fn the_issue_sentence_comes_from_the_one_validator() {
        let mut values = vless_values();
        values.uuid.clear();
        assert_eq!(
            values_issue(&values).as_deref(),
            Some("uuid is required for vless")
        );
    }

    #[test]
    fn a_patch_that_does_not_parse_is_named_before_the_daemon_is_asked() {
        let mut values = vless_values();
        values.patch = "{ not json".to_string();
        let issue = values_issue(&values).expect("a bad patch is an issue");
        assert!(
            issue.starts_with("outbound_patch does not parse as JSON"),
            "{issue}"
        );
    }

    /// A comma-separated alpn row becomes a list; the bandwidth rows accept
    /// the same spellings subscriptions do.
    #[test]
    fn list_and_bandwidth_rows_are_normalized_like_imports() {
        let values = DialogValues {
            name: "Typed".to_string(),
            protocol_index: 4, // hysteria2
            address: "server.example.invalid".to_string(),
            port: 443,
            auth: "invented".to_string(),
            h2_alpn: "h3, h2".to_string(),
            h2_up: "100 Mbps".to_string(),
            ..DialogValues::default()
        };
        let draft = draft_from_values(&values).expect("assembles");
        let settings = draft.hysteria2.expect("hysteria2 block");
        assert_eq!(
            settings.alpn,
            Some(vec!["h3".to_string(), "h2".to_string()])
        );
        assert_eq!(settings.up_mbps, Some(100));
        assert!(draft.stream.is_none(), "no stream block for hysteria2");
    }

    #[test]
    fn editing_a_server_preserves_unmodeled_fields() {
        let pin = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let link = format!(
            "vless://11111111-2222-3333-4444-555555555555@server.example.invalid:443\
             ?encryption=mlkem768&type=xhttp&mode=packet-up&security=tls&sni=cover.example.invalid\
             &pinSHA256={pin}&extra=%7B%22xPaddingBytes%22%3A%22100-200%22%7D#Original"
        );
        let server = oxidom_core::link::parse_link(&link).expect("parses");
        let mut values = values_from_server(&server);
        values.port = 8443;
        let edited = draft_from_values(&values).expect("assembles");
        assert_eq!(edited.port, 8443);
        assert_eq!(edited.encryption.as_deref(), Some("mlkem768"));
        let stream = edited.stream.expect("stream");
        assert_eq!(stream.pin_sha256.as_deref(), Some(pin));
        assert_eq!(stream.xhttp_mode.as_deref(), Some("packet-up"));
        assert_eq!(
            stream.xhttp_extra,
            Some(serde_json::json!({"xPaddingBytes": "100-200"}))
        );
    }

    #[test]
    fn validation_errors_identify_the_field_that_prevents_saving() {
        assert_eq!(invalid_field(None), None);
        assert_eq!(invalid_field(Some("address is empty")), Some("address"));
        assert_eq!(invalid_field(Some("port is 0")), Some("port"));
        assert_eq!(
            invalid_field(Some("uuid is required for vless")),
            Some("uuid")
        );
        assert_eq!(
            invalid_field(Some("password is required for trojan")),
            Some("password")
        );
        assert_eq!(
            invalid_field(Some("method is required for shadowsocks")),
            Some("method")
        );
        assert_eq!(
            invalid_field(Some("auth is required for hysteria2")),
            Some("auth")
        );
        assert_eq!(
            invalid_field(Some("outbound_patch does not parse as JSON: EOF")),
            Some("outbound_patch")
        );
        assert_eq!(
            invalid_field(Some("outbound_patch is not a JSON object")),
            Some("outbound_patch")
        );
        assert_eq!(
            invalid_field(Some(
                "outbound_patch must not set \"tag\"; that is what the typed fields are for"
            )),
            Some("outbound_patch")
        );
        assert_eq!(
            invalid_field(Some("the draft does not generate an outbound")),
            None
        );
    }

    #[test]
    fn transport_editing_shows_applicable_fields_while_preserving_populated_rows() {
        assert!(stream_field_visible(
            "sni",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));
        assert!(stream_field_visible(
            "alpn",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));
        assert!(stream_field_visible(
            "path",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));
        assert!(stream_field_visible(
            "host",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));
        assert!(stream_field_visible(
            "flow",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));
        assert!(!stream_field_visible(
            "service_name",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));
        assert!(!stream_field_visible(
            "header_type",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));
        assert!(!stream_field_visible(
            "public_key",
            Protocol::Vless,
            "ws",
            "tls",
            ""
        ));

        assert!(stream_field_visible(
            "service_name",
            Protocol::Vless,
            "grpc",
            "reality",
            ""
        ));
        assert!(stream_field_visible(
            "public_key",
            Protocol::Vless,
            "grpc",
            "reality",
            ""
        ));
        assert!(stream_field_visible(
            "short_id",
            Protocol::Vless,
            "grpc",
            "reality",
            ""
        ));
        assert!(stream_field_visible(
            "spider_x",
            Protocol::Vless,
            "grpc",
            "reality",
            ""
        ));
        assert!(!stream_field_visible(
            "alpn",
            Protocol::Vless,
            "grpc",
            "reality",
            ""
        ));
        assert!(!stream_field_visible(
            "path",
            Protocol::Vless,
            "grpc",
            "reality",
            ""
        ));
        assert!(!stream_field_visible(
            "host",
            Protocol::Vless,
            "grpc",
            "reality",
            ""
        ));

        assert!(stream_field_visible(
            "header_type",
            Protocol::Vless,
            "tcp",
            "none",
            ""
        ));
        assert!(stream_field_visible(
            "path",
            Protocol::Vless,
            "tcp",
            "none",
            ""
        ));
        assert!(stream_field_visible(
            "host",
            Protocol::Vless,
            "tcp",
            "none",
            ""
        ));
        assert!(!stream_field_visible(
            "sni",
            Protocol::Vless,
            "tcp",
            "none",
            ""
        ));
        assert!(!stream_field_visible(
            "flow",
            Protocol::Trojan,
            "tcp",
            "tls",
            ""
        ));

        assert!(
            stream_field_visible("service_name", Protocol::Vless, "ws", "tls", "retained-svc"),
            "a populated field must stay visible even when another transport is selected"
        );
        assert!(
            stream_field_visible("public_key", Protocol::Vless, "tcp", "none", "invented-pbk"),
            "a populated reality key must stay visible when security is switched"
        );
    }
}
