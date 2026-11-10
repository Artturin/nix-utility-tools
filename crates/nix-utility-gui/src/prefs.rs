//! The preferences window for the analyzer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use nix_log_diff_analyzer::CancelToken;
use nix_log_diff_analyzer::llm::{self, Timeouts};
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use tokio::sync::oneshot;

use crate::analysis::{AnalysisPrefs, AnalyzerSettings, AutoTiming, Endpoint};
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

const MODELS_HINT: &str = "Press refresh to list what the server offers";
const KEY_HINT: &str = "Type a key and press the apply button to store it";

fn endpoint_index(endpoint: Endpoint) -> u32 {
    Endpoint::ALL
        .iter()
        .position(|&e| e == endpoint)
        .unwrap_or(0) as u32
}

fn timing_index(timing: AutoTiming) -> u32 {
    match timing {
        AutoTiming::AfterRun => 0,
        AutoTiming::DuringRun => 1,
    }
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

pub fn show(
    window: &adw::ApplicationWindow,
    initial: AnalysisPrefs,
    keys: Arc<dyn KeyStore>,
    on_change: impl Fn(AnalysisPrefs) + 'static,
    on_closed: impl Fn() + 'static,
) {
    let current = initial.active().clone();
    let state = Rc::new(RefCell::new(initial.clone()));
    // Set while the rows are filled from another profile, so that filling
    // them is not taken for an edit.
    let loading = Rc::new(Cell::new(false));
    // Bumped on every profile switch; a model list fetched for an earlier
    // profile is then dropped.
    let generation = Rc::new(Cell::new(0u64));
    let on_change: Rc<dyn Fn(AnalysisPrefs)> = Rc::new(on_change);
    let dialog = adw::PreferencesDialog::builder()
        .title("Analysis preferences")
        .build();
    let page = adw::PreferencesPage::new();

    // Profile.
    let profile_group = adw::PreferencesGroup::builder()
        .title("Profile")
        .description(
            "Everything below except the privacy setting belongs to the profile. \
             The header bar shows which profile is in use and switches between them.",
        )
        .build();
    let names = gtk::StringList::new(&[]);
    let profile = adw::ComboRow::builder()
        .title("Profile in use")
        .model(&names)
        .build();
    let name = adw::EntryRow::builder()
        .title("Name")
        .show_apply_button(true)
        .build();
    let add = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("New profile, copied from this one")
        .css_classes(["flat"])
        .build();
    let remove = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete this profile")
        .css_classes(["flat"])
        .build();
    let profile_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    profile_buttons.append(&add);
    profile_buttons.append(&remove);
    profile_group.set_header_suffix(Some(&profile_buttons));
    profile_group.add(&profile);
    profile_group.add(&name);
    page.add(&profile_group);

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
        .selected(endpoint_index(current.endpoint))
        .build();
    let host = adw::EntryRow::builder()
        .title("Host (empty for the default address)")
        .text(&current.host)
        .build();
    let model = adw::EntryRow::builder()
        .title("Model")
        .text(&current.model)
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
        .subtitle(MODELS_HINT)
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
            "Stored in the desktop's secret service, never in the settings file, and \
             shared by profiles with the same server type and host. \
             Not needed for local servers.",
        )
        .build();
    let key_entry = adw::PasswordEntryRow::builder()
        .title("API key for this server")
        .show_apply_button(true)
        .build();
    let key_status = adw::ActionRow::builder()
        .title("Status")
        .subtitle(KEY_HINT)
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
        f64::from(current.context_tokens),
    );
    let thinking = spin(
        "Maximum thinking tokens",
        "0 for no limit; stops a model that thinks for too long",
        1_000_000.0,
        256.0,
        f64::from(current.max_thinking_tokens),
    );
    let output = spin(
        "Maximum answer tokens",
        "0 for no limit",
        1_000_000.0,
        64.0,
        f64::from(current.max_output_tokens),
    );
    let idle = spin(
        "Silence timeout (seconds)",
        "Give up when the server sends nothing for this long; 0 to wait forever",
        86_400.0,
        30.0,
        current.idle_timeout_secs as f64,
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
        .active(current.auto_analyze)
        .build();
    let timing = adw::ComboRow::builder()
        .title("When")
        .model(&gtk::StringList::new(&[
            "After the run finishes",
            "As each build log finishes",
        ]))
        .selected(timing_index(current.auto_timing))
        .build();
    let cap = spin(
        "Most packages per run",
        "0 for no limit; the others keep their Analyze button",
        100_000.0,
        5.0,
        f64::from(current.auto_cap),
    );
    for row in [
        auto_on.upcast_ref::<gtk::Widget>(),
        timing.upcast_ref(),
        cap.upcast_ref(),
    ] {
        auto.add(row);
    }
    page.add(&auto);

    // Privacy, shared by every profile.
    let privacy = adw::PreferencesGroup::builder()
        .title("Privacy")
        .description("The same for every profile.")
        .build();
    let save_host = adw::SwitchRow::builder()
        .title("Save the host in analysis files")
        .subtitle("Off: a shared results directory does not reveal the server's address")
        .active(initial.save_host)
        .build();
    privacy.add(&save_host);
    page.add(&privacy);
    dialog.add(&page);

    // Whatever changed, tell the window and refresh what is derived.
    let refresh = {
        let (state, limit, context) = (Rc::clone(&state), limit.clone(), context.clone());
        Rc::new(move || {
            let state = state.borrow();
            let profile = state.active();
            context.set_subtitle(&format!(
                "0 uses the default for this server type ({}). Match the server's own setting.",
                profile.endpoint.default_context()
            ));
            limit.set_subtitle(&format!(
                "{} {}",
                profile.limit_text(),
                profile.context_advice()
            ));
        })
    };
    refresh();
    let commit = {
        let (state, refresh, loading) =
            (Rc::clone(&state), Rc::clone(&refresh), Rc::clone(&loading));
        let on_change = Rc::clone(&on_change);
        Rc::new(move |change: &dyn Fn(&mut AnalysisPrefs)| {
            if loading.get() {
                return;
            }
            change(&mut state.borrow_mut());
            refresh();
            on_change(state.borrow().clone());
        })
    };

    // Fill every row from the active profile, after switching to another.
    let load = {
        let (state, loading, generation, refresh) = (
            Rc::clone(&state),
            Rc::clone(&loading),
            Rc::clone(&generation),
            Rc::clone(&refresh),
        );
        let (names, profile, name, remove) =
            (names.clone(), profile.clone(), name.clone(), remove.clone());
        let (endpoint, host, model) = (endpoint.clone(), host.clone(), model.clone());
        let (models_list, models_button, models_status, models_popover) = (
            models_list.clone(),
            models_button.clone(),
            models_status.clone(),
            models_popover.clone(),
        );
        let (key_entry, key_status) = (key_entry.clone(), key_status.clone());
        let (context, thinking, output, idle) = (
            context.clone(),
            thinking.clone(),
            output.clone(),
            idle.clone(),
        );
        let (auto_on, timing, cap) = (auto_on.clone(), timing.clone(), cap.clone());
        Rc::new(move || {
            loading.set(true);
            generation.set(generation.get() + 1);
            let prefs = state.borrow().clone();
            let current = prefs.active();
            let all = prefs.names();
            let refs: Vec<&str> = all.iter().map(String::as_str).collect();
            names.splice(0, names.n_items(), &refs);
            profile.set_selected(all.iter().position(|n| *n == current.name).unwrap_or(0) as u32);
            name.set_text(&current.name);
            name.remove_css_class("error");
            remove.set_sensitive(prefs.profiles.len() > 1);
            endpoint.set_selected(endpoint_index(current.endpoint));
            host.set_text(&current.host);
            model.set_text(&current.model);
            models_popover.popdown();
            clear(&models_list);
            models_button.set_sensitive(false);
            models_status.set_subtitle(MODELS_HINT);
            key_entry.set_text("");
            key_status.set_subtitle(KEY_HINT);
            context.set_value(f64::from(current.context_tokens));
            thinking.set_value(f64::from(current.max_thinking_tokens));
            output.set_value(f64::from(current.max_output_tokens));
            idle.set_value(current.idle_timeout_secs as f64);
            auto_on.set_active(current.auto_analyze);
            timing.set_selected(timing_index(current.auto_timing));
            cap.set_value(f64::from(current.auto_cap));
            loading.set(false);
            refresh();
        })
    };
    load();

    // Switching, copying, deleting and renaming profiles.
    let switched = {
        let (state, load, on_change) = (Rc::clone(&state), Rc::clone(&load), Rc::clone(&on_change));
        Rc::new(move |change: &dyn Fn(&mut AnalysisPrefs)| {
            change(&mut state.borrow_mut());
            load();
            on_change(state.borrow().clone());
        })
    };
    {
        let (switched, loading) = (Rc::clone(&switched), Rc::clone(&loading));
        profile.connect_selected_notify(move |row| {
            if loading.get() {
                return;
            }
            let Some(chosen) = row
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|item| item.string().to_string())
            else {
                return;
            };
            switched(&|prefs| {
                prefs.select(&chosen);
            });
        });
    }
    {
        let switched = Rc::clone(&switched);
        add.connect_clicked(move |_| {
            switched(&|prefs| {
                prefs.add_copy();
            })
        });
    }
    {
        let (switched, state, window) = (Rc::clone(&switched), Rc::clone(&state), dialog.clone());
        remove.connect_clicked(move |_| {
            let confirm = adw::AlertDialog::new(
                Some(&format!("Delete the profile “{}”?", state.borrow().active)),
                Some("Its API key stays in the secret service."),
            );
            let switched = Rc::clone(&switched);
            confirm.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            confirm.set_default_response(Some("cancel"));
            confirm.set_close_response("cancel");
            confirm.connect_response(None, move |_, response| {
                if response == "delete" {
                    switched(&|prefs| {
                        prefs.remove_active();
                    });
                }
            });
            confirm.present(Some(&window));
        });
    }
    {
        let (state, loading, names, profile) = (
            Rc::clone(&state),
            Rc::clone(&loading),
            names.clone(),
            profile.clone(),
        );
        let on_change = Rc::clone(&on_change);
        // Renamed on apply only, so that a half-typed name is never saved.
        name.connect_changed(|row| {
            row.remove_css_class("error");
            row.set_tooltip_text(None);
        });
        name.connect_apply(move |row| {
            let result = state.borrow_mut().rename_active(&row.text());
            match result {
                Ok(()) => {
                    row.remove_css_class("error");
                    row.set_tooltip_text(None);
                    // Relabel the profile in the list without switching.
                    let index = profile.selected();
                    loading.set(true);
                    names.splice(index, 1, &[state.borrow().active.as_str()]);
                    profile.set_selected(index);
                    loading.set(false);
                    on_change(state.borrow().clone());
                }
                Err(why) => {
                    row.add_css_class("error");
                    row.set_tooltip_text(Some(why));
                }
            }
        });
    }

    // Rows of the active profile.
    {
        let commit = Rc::clone(&commit);
        endpoint.connect_selected_notify(move |row| {
            let chosen = Endpoint::ALL[(row.selected() as usize).min(Endpoint::ALL.len() - 1)];
            commit(&|p| p.active_mut().endpoint = chosen);
        });
    }
    {
        let commit = Rc::clone(&commit);
        host.connect_changed(move |row| {
            let text = row.text().to_string();
            commit(&|p| p.active_mut().host = text.clone());
        });
    }
    {
        let commit = Rc::clone(&commit);
        model.connect_changed(move |row| {
            let text = row.text().to_string();
            commit(&|p| p.active_mut().model = text.trim().to_owned());
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
            commit(&|p| set(p.active_mut(), value));
        });
    }
    {
        let commit = Rc::clone(&commit);
        idle.connect_value_notify(move |row| {
            let value = row.value() as u64;
            commit(&|p| p.active_mut().idle_timeout_secs = value);
        });
    }
    {
        let commit = Rc::clone(&commit);
        auto_on.connect_active_notify(move |row| {
            let value = row.is_active();
            commit(&|p| p.active_mut().auto_analyze = value);
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
            commit(&|p| p.active_mut().auto_timing = value);
        });
    }
    {
        let commit = Rc::clone(&commit);
        save_host.connect_active_notify(move |row| {
            let value = row.is_active();
            commit(&|p| p.save_host = value);
        });
    }

    // Models.
    {
        let (state, keys, generation) =
            (Rc::clone(&state), Arc::clone(&keys), Rc::clone(&generation));
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
            let (settings, keys) = (state.borrow().active().clone(), Arc::clone(&keys));
            let asked_in = generation.get();
            let generation = Rc::clone(&generation);
            let (fetch, status, list, button) =
                (fetch.clone(), status.clone(), list.clone(), button.clone());
            in_background(
                move || fetch_models(&settings, keys.as_ref()),
                move |result| {
                    fetch.set_sensitive(true);
                    if generation.get() != asked_in {
                        return;
                    }
                    clear(&list);
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
            let account = key_account(state.borrow().active());
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
            let account = key_account(state.borrow().active());
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

    // Every keystroke is a change; saving waits until the dialog is closed.
    dialog.connect_closed(move |_| on_closed());
    dialog.present(Some(window));
}
