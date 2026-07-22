use adw::prelude::*;

use crate::config::{Config, LatencyMethod};

#[derive(Debug, Clone)]
pub struct SettingsValues {
    pub socks_port: u16,
    pub http_port: u16,
    pub system_proxy: bool,
    pub latency_method: LatencyMethod,
    pub latency_test_url: String,
}

pub struct SettingsView {
    pub root: adw::PreferencesPage,
}

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

        let root = adw::PreferencesPage::new();
        root.add(&proxy_group);
        root.add(&latency_group);

        let socks_value = socks.clone();
        let http_value = http.clone();
        let system_proxy_value = system_proxy.clone();
        let method_value = method.clone();
        let test_url_value = test_url.clone();
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
        test_url.connect_changed(move |_| emit());

        Self { root }
    }
}
