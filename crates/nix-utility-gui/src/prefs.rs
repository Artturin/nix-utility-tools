//! The preferences window for the analyzer.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use nix_log_diff_analyzer::CancelToken;
use nix_log_diff_analyzer::llm::{self, Timeouts};
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use tokio::sync::oneshot;

use crate::analysis::{AnalyzerSettings, AutoTiming, Endpoint};
use crate::keystore::{KeyStore, Stored};
use crate::runner::key_account;

/// Run blocking `work` on a thread and await its result on the main loop.
fn in_background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    gtk::glib::MainContext::default().spawn_local(async move {
        if let Ok(value) = rx.await {
            done(value);
        }
    });
}

fn fetch_models(settings: &AnalyzerSettings, keys: &dyn KeyStore) -> Result<Vec<String>, String> {
    let key = keys
        .get(&key_account(settings))
        .map_err(|err| format!("cannot read the API key: {err}"))?;
    let host = Some(settings.host.trim()).filter(|host| !host.is_empty());
    let timeouts = Timeouts {
        idle: Some(Duration::from_secs(15)),
        ..Timeouts::default()
    };
    let client = llm::connect(settings.endpoint.llm(), host, key.as_deref(), timeouts)
        .map_err(|err| format!("{err:#}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| err.to_string())?;
    runtime
        .block_on(client.list_models(&CancelToken::new()))
        .map_err(|err| format!("{err:#}"))?
        .ok_or_else(|| "cancelled".to_owned())
}

fn spin(title: &str, subtitle: &str, max: f64, step: f64, value: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(0.0, max, step);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_value(value);
    row
}

pub fn show(
    window: &adw::ApplicationWindow,
    initial: AnalyzerSettings,
    keys: Arc<dyn KeyStore>,
    on_change: impl Fn(AnalyzerSettings) + 'static,
) {
    let state = Rc::new(RefCell::new(initial.clone()));
    let dialog = adw::PreferencesDialog::builder()
        .title("Analysis preferences")
        .build();
    let page = adw::PreferencesPage::new();

    // Server.
    let server = adw::PreferencesGroup::builder()
        .title("Language model")
        .description("Any server with an OpenAI-compatible API. Build log diffs are sent to it.")
        .build();
    let endpoint = adw::ComboRow::builder()
        .title("Server type")
        .model(&gtk::StringList::new(
            &Endpoint::ALL.map(|endpoint| endpoint.label()),
        ))
        .selected(
            Endpoint::ALL
                .iter()
                .position(|&e| e == initial.endpoint)
                .unwrap_or(0) as u32,
        )
        .build();
    let host = adw::EntryRow::builder()
        .title("Host (empty for the default address)")
        .text(&initial.host)
        .build();
    let model = adw::EntryRow::builder()
        .title("Model")
        .text(&initial.model)
        .build();
    let fetch = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Ask the server which models it has")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let models_list = gtk::ListBox::builder()
        .css_classes(["navigation-sidebar"])
        .build();
    let models_popover = gtk::Popover::builder()
        .child(
            &gtk::ScrolledWindow::builder()
                .child(&models_list)
                .min_content_width(260)
                .max_content_height(300)
                .propagate_natural_height(true)
                .build(),
        )
        .build();
    let models_button = gtk::MenuButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("Models the server offers")
        .popover(&models_popover)
        .valign(gtk::Align::Center)
        .sensitive(false)
        .css_classes(["flat"])
        .build();
    model.add_suffix(&fetch);
    model.add_suffix(&models_button);
    let models_status = adw::ActionRow::builder()
        .title("Models")
        .subtitle("Press refresh to list what the server offers")
        .subtitle_lines(0)
        .build();
    for row in [
        endpoint.upcast_ref::<gtk::Widget>(),
        host.upcast_ref(),
        model.upcast_ref(),
        models_status.upcast_ref(),
    ] {
        server.add(row);
    }
    page.add(&server);

    // API key.
    let key_group = adw::PreferencesGroup::builder()
        .title("API key")
        .description(
            "Stored in the desktop's secret service, never in the settings file. \
             Not needed for local servers.",
        )
        .build();
    let key_entry = adw::PasswordEntryRow::builder()
        .title("API key for this server")
        .show_apply_button(true)
        .build();
    let key_status = adw::ActionRow::builder()
        .title("Status")
        .subtitle("Type a key and press the apply button to store it")
        .subtitle_lines(0)
        .build();
    let key_remove = gtk::Button::builder()
        .label("Remove")
        .valign(gtk::Align::Center)
        .build();
    key_status.add_suffix(&key_remove);
    key_group.add(&key_entry);
    key_group.add(&key_status);
    page.add(&key_group);

    // Size and budgets.
    let size = adw::PreferencesGroup::builder()
        .title("Size and budgets")
        .build();
    let context = spin(
        "Context size (tokens)",
        "",
        4_000_000.0,
        256.0,
        f64::from(initial.context_tokens),
    );
    let thinking = spin(
        "Maximum thinking tokens",
        "0 for no limit; stops a model that thinks for too long",
        1_000_000.0,
        256.0,
        f64::from(initial.max_thinking_tokens),
    );
    let output = spin(
        "Maximum answer tokens",
        "0 for no limit",
        1_000_000.0,
        64.0,
        f64::from(initial.max_output_tokens),
    );
    let idle = spin(
        "Silence timeout (seconds)",
        "Give up when the server sends nothing for this long; 0 to wait forever",
        86_400.0,
        30.0,
        initial.idle_timeout_secs as f64,
    );
    let limit = adw::ActionRow::builder()
        .title("Largest diff")
        .subtitle_lines(0)
        .build();
    for row in [&context, &thinking, &output, &idle] {
        size.add(row);
    }
    size.add(&limit);
    page.add(&size);

    // Automatic analysis.
    let auto = adw::PreferencesGroup::builder()
        .title("Automatic analysis")
        .build();
    let auto_on = adw::SwitchRow::builder()
        .title("Analyze every changed build log")
        .subtitle("One at a time. Off: use the Analyze button on a package")
        .active(initial.auto_analyze)
        .build();
    let timing = adw::ComboRow::builder()
        .title("When")
        .model(&gtk::StringList::new(&[
            "After the run finishes",
            "As each build log finishes",
        ]))
        .selected(match initial.auto_timing {
            AutoTiming::AfterRun => 0,
            AutoTiming::DuringRun => 1,
        })
        .build();
    let cap = spin(
        "Most packages per run",
        "0 for no limit; the others keep their Analyze button",
        100_000.0,
        5.0,
        f64::from(initial.auto_cap),
    );
    let save_host = adw::SwitchRow::builder()
        .title("Save the host in analysis files")
        .subtitle("Off: a shared results directory does not reveal the server's address")
        .active(initial.save_host)
        .build();
    for row in [
        auto_on.upcast_ref::<gtk::Widget>(),
        timing.upcast_ref(),
        cap.upcast_ref(),
        save_host.upcast_ref(),
    ] {
        auto.add(row);
    }
    page.add(&auto);
    dialog.add(&page);

    // Whatever changed, tell the window and refresh what is derived.
    let refresh = {
        let (state, limit, context) = (Rc::clone(&state), limit.clone(), context.clone());
        Rc::new(move || {
            let state = state.borrow();
            context.set_subtitle(&format!(
                "0 uses the default for this server type ({}). Match the server's own setting.",
                state.endpoint.default_context()
            ));
            limit.set_subtitle(&format!(
                "{} {}",
                state.limit_text(),
                state.context_advice()
            ));
        })
    };
    refresh();
    let commit = {
        let (state, refresh) = (Rc::clone(&state), Rc::clone(&refresh));
        let on_change = Rc::new(on_change);
        Rc::new(move |change: &dyn Fn(&mut AnalyzerSettings)| {
            change(&mut state.borrow_mut());
            refresh();
            on_change(state.borrow().clone());
        })
    };

    {
        let commit = Rc::clone(&commit);
        endpoint.connect_selected_notify(move |row| {
            let chosen = Endpoint::ALL[(row.selected() as usize).min(Endpoint::ALL.len() - 1)];
            commit(&|s| s.endpoint = chosen);
        });
    }
    {
        let commit = Rc::clone(&commit);
        host.connect_changed(move |row| {
            let text = row.text().to_string();
            commit(&|s| s.host = text.clone());
        });
    }
    {
        let commit = Rc::clone(&commit);
        model.connect_changed(move |row| {
            let text = row.text().to_string();
            commit(&|s| s.model = text.trim().to_owned());
        });
    }
    for (row, set) in [
        (
            &context,
            (|s: &mut AnalyzerSettings, v: u32| s.context_tokens = v) as fn(&mut _, u32),
        ),
        (&thinking, |s, v| s.max_thinking_tokens = v),
        (&output, |s, v| s.max_output_tokens = v),
        (&cap, |s, v| s.auto_cap = v),
    ] {
        let commit = Rc::clone(&commit);
        row.connect_value_notify(move |row| {
            let value = row.value() as u32;
            commit(&|s| set(s, value));
        });
    }
    {
        let commit = Rc::clone(&commit);
        idle.connect_value_notify(move |row| {
            let value = row.value() as u64;
            commit(&|s| s.idle_timeout_secs = value);
        });
    }
    {
        let commit = Rc::clone(&commit);
        auto_on.connect_active_notify(move |row| {
            let value = row.is_active();
            commit(&|s| s.auto_analyze = value);
        });
    }
    {
        let commit = Rc::clone(&commit);
        timing.connect_selected_notify(move |row| {
            let value = if row.selected() == 1 {
                AutoTiming::DuringRun
            } else {
                AutoTiming::AfterRun
            };
            commit(&|s| s.auto_timing = value);
        });
    }
    {
        let commit = Rc::clone(&commit);
        save_host.connect_active_notify(move |row| {
            let value = row.is_active();
            commit(&|s| s.save_host = value);
        });
    }

    // Models.
    {
        let (state, keys) = (Rc::clone(&state), Arc::clone(&keys));
        let (status, list, button, popover, model) = (
            models_status.clone(),
            models_list.clone(),
            models_button.clone(),
            models_popover.clone(),
            model.clone(),
        );
        {
            let (model, popover) = (model.clone(), popover.clone());
            list.connect_row_activated(move |_, row| {
                model.set_text(row.widget_name().as_str());
                popover.popdown();
            });
        }
        fetch.connect_clicked(move |fetch| {
            fetch.set_sensitive(false);
            status.set_subtitle("Asking the server…");
            let (settings, keys) = (state.borrow().clone(), Arc::clone(&keys));
            let (fetch, status, list, button) =
                (fetch.clone(), status.clone(), list.clone(), button.clone());
            in_background(
                move || fetch_models(&settings, keys.as_ref()),
                move |result| {
                    fetch.set_sensitive(true);
                    while let Some(child) = list.first_child() {
                        list.remove(&child);
                    }
                    match result {
                        Ok(models) if models.is_empty() => {
                            status.set_subtitle("The server offers no models");
                            button.set_sensitive(false);
                        }
                        Ok(models) => {
                            status.set_subtitle(&format!("{} models found", models.len()));
                            for name in &models {
                                let row = gtk::ListBoxRow::new();
                                row.set_widget_name(name);
                                row.set_child(Some(
                                    &gtk::Label::builder()
                                        .label(name)
                                        .xalign(0.0)
                                        .margin_top(6)
                                        .margin_bottom(6)
                                        .margin_start(6)
                                        .margin_end(6)
                                        .build(),
                                ));
                                list.append(&row);
                            }
                            button.set_sensitive(true);
                        }
                        Err(message) => {
                            status.set_subtitle(&format!("Could not list models: {message}"));
                            button.set_sensitive(false);
                        }
                    }
                },
            );
        });
    }

    // Key.
    {
        let (state, keys, status) = (Rc::clone(&state), Arc::clone(&keys), key_status.clone());
        key_entry.connect_apply(move |row| {
            let key = row.text().to_string();
            if key.is_empty() {
                return;
            }
            let account = key_account(&state.borrow());
            let keys = Arc::clone(&keys);
            status.set_subtitle("Storing…");
            let (status, row) = (status.clone(), row.clone());
            in_background(
                move || keys.set(&account, &key),
                move |result| {
                    row.set_text("");
                    status.set_subtitle(&match result {
                        Ok(Stored::Saved) => "Stored in the secret service".to_owned(),
                        Ok(Stored::SessionOnly(why)) => {
                            format!("Kept until the app closes only: {why}")
                        }
                        Err(err) => format!("Could not store the key: {err}"),
                    });
                },
            );
        });
    }
    {
        let (state, keys, status) = (Rc::clone(&state), Arc::clone(&keys), key_status.clone());
        key_remove.connect_clicked(move |_| {
            let account = key_account(&state.borrow());
            let keys = Arc::clone(&keys);
            let status = status.clone();
            in_background(
                move || keys.delete(&account),
                move |result| {
                    status.set_subtitle(&match result {
                        Ok(()) => "No key stored for this server".to_owned(),
                        Err(err) => format!("Could not remove the key: {err}"),
                    });
                },
            );
        });
    }

    dialog.present(Some(window));
}
