use adw::prelude::*;

use crate::config::{Config, LatencyMethod};

#[derive(Debug, Clone)]
pub struct SettingsValues {
    pub socks_port: u16,
    pub http_port: u16,
    pub system_proxy: bool,
    pub latency_method: LatencyMethod,
    pub latency_test_url: String,
    pub subscription_user_agent: String,
}

pub struct SettingsView {
    pub root: adw::PreferencesPage,
}

/// Recognized subscription client identifiers. Picking one fills the editable
/// User-Agent field; the field itself stays the source of truth so users can
/// still type a value not listed here.
const UA_PRESETS: &[(&str, &str)] = &[
    ("v2rayNG", "v2rayNG/1.9.5"),
    ("Happ", "Happ/3.13.0"),
    ("v2rayN", "v2rayN/6.45"),
    ("Streisand", "Streisand"),
    ("Hiddify", "Hiddify/2.0.5"),
    ("NekoBox", "NekoBox/1.3.5"),
    ("Shadowrocket", "Shadowrocket/2.2.9"),
    ("Clash Meta", "clash-verge/1.7.7"),
    ("sing-box", "SFA/1.10.0"),
];

impl SettingsView {
    pub fn new(config: &Config, on_change: impl Fn(SettingsValues) + Clone + 'static) -> Self {
        let socks = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
        socks.set_title("SOCKS port");
        socks.set_value(f64::from(config.socks_port));
        let http = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
        http.set_title("HTTP port");
        http.set_value(f64::from(config.http_port));
        let system_proxy = adw::SwitchRow::builder()
            .title("System proxy")
            .subtitle("Configure the desktop proxy while connected")
            .active(config.system_proxy)
            .build();

        let methods = gtk::StringList::new(&["ICMP", "TCP", "HTTP HEAD", "HTTP GET"]);
        let method = adw::ComboRow::builder()
            .title("Latency method")
            .model(&methods)
            .selected(match config.latency_method {
                LatencyMethod::Icmp => 0,
                LatencyMethod::Tcp => 1,
                LatencyMethod::HttpHead => 2,
                LatencyMethod::HttpGet => 3,
            })
            .build();
        let test_url = adw::EntryRow::builder()
            .title("Latency test URL")
            .text(&config.latency_test_url)
            .build();
        let user_agent = adw::EntryRow::builder()
            .title("Subscription User-Agent")
            .text(&config.subscription_user_agent)
            .build();
        let preset_labels: Vec<&str> = std::iter::once("Custom")
            .chain(UA_PRESETS.iter().map(|(label, _)| *label))
            .collect();
        let presets = gtk::StringList::new(&preset_labels);
        // Preselect the preset matching the saved value, else "Custom" (index 0).
        let selected_preset = UA_PRESETS
            .iter()
            .position(|(_, ua)| *ua == config.subscription_user_agent)
            .map(|i| i as u32 + 1)
            .unwrap_or(0);
        let ua_preset = adw::ComboRow::builder()
            .title("Client preset")
            .subtitle("Fills the User-Agent below")
            .model(&presets)
            .selected(selected_preset)
            .build();

        let proxy_group = adw::PreferencesGroup::builder()
            .title("Local proxy")
            .build();
        proxy_group.add(&socks);
        proxy_group.add(&http);
        proxy_group.add(&system_proxy);
        let latency_group = adw::PreferencesGroup::builder()
            .title("Latency")
            .description("HTTP checks use the active local SOCKS proxy")
            .build();
        latency_group.add(&method);
        latency_group.add(&test_url);
        let subscription_group = adw::PreferencesGroup::builder()
            .title("Subscription")
            .description("Some panels only return configs to recognized clients; change this if a subscription reports \"app not supported\"")
            .build();
        subscription_group.add(&ua_preset);
        subscription_group.add(&user_agent);

        let root = adw::PreferencesPage::new();
        root.add(&proxy_group);
        root.add(&latency_group);
        root.add(&subscription_group);

        let socks_value = socks.clone();
        let http_value = http.clone();
        let system_proxy_value = system_proxy.clone();
        let method_value = method.clone();
        let test_url_value = test_url.clone();
        let user_agent_value = user_agent.clone();
        let emit = move || {
            on_change(SettingsValues {
                socks_port: socks_value.value() as u16,
                http_port: http_value.value() as u16,
                system_proxy: system_proxy_value.is_active(),
                latency_method: match method_value.selected() {
                    0 => LatencyMethod::Icmp,
                    1 => LatencyMethod::Tcp,
                    2 => LatencyMethod::HttpHead,
                    _ => LatencyMethod::HttpGet,
                },
                latency_test_url: test_url_value.text().to_string(),
                subscription_user_agent: user_agent_value.text().to_string(),
            });
        };
        let emit = std::rc::Rc::new(emit);
        socks.connect_value_notify({
            let emit = emit.clone();
            move |_| emit()
        });
        http.connect_value_notify({
            let emit = emit.clone();
            move |_| emit()
        });
        system_proxy.connect_active_notify({
            let emit = emit.clone();
            move |_| emit()
        });
        method.connect_selected_notify({
            let emit = emit.clone();
            move |_| emit()
        });
        test_url.connect_changed({
            let emit = emit.clone();
            move |_| emit()
        });
        // Selecting a preset writes its UA into the entry (the emitting field).
        // Index 0 is "Custom" and leaves the entry untouched.
        ua_preset.connect_selected_notify({
            let user_agent = user_agent.clone();
            move |row| {
                if let Some((_, ua)) = UA_PRESETS.get(row.selected().wrapping_sub(1) as usize) {
                    if user_agent.text() != *ua {
                        user_agent.set_text(ua);
                    }
                }
            }
        });
        // Editing the entry moves the preset to whichever entry matches, else Custom.
        user_agent.connect_changed({
            let ua_preset = ua_preset.clone();
            let emit = emit.clone();
            move |entry| {
                let text = entry.text();
                let idx = UA_PRESETS
                    .iter()
                    .position(|(_, ua)| *ua == text)
                    .map(|i| i as u32 + 1)
                    .unwrap_or(0);
                if ua_preset.selected() != idx {
                    ua_preset.set_selected(idx);
                }
                emit();
            }
        });

        Self { root }
    }
}
