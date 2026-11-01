//! The settings page inside the overlay: search indexing, clipboard
//! history, and general (hotkeys, start with Windows, data folder). Every
//! change saves at once; folder changes need a rebuild, which is one click.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dash_search::search::{self as s, FileSearchIndexOptions, FileSearchStatus};
use gpui::{div, prelude::*, px, App, Context, Entity, PathPromptOptions, ScrollHandle, SharedString, Window};

use crate::input::TextInput;
use crate::theme;

/// True while a folder picker is open, so losing focus to it doesn't hide the overlay.
pub static PICKING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Search,
    Clipboard,
    General,
}

#[derive(Clone, Copy)]
enum Folders {
    Names,
    Inside,
    Skipped,
    Ocr,
}

pub struct Settings {
    section: Section,
    options: Option<FileSearchIndexOptions>,
    status: Option<FileSearchStatus>,
    extensions: Entity<TextInput>,
    new_app: Entity<TextInput>,
    confirm_clear: bool,
    login: bool,
    /// OCR and the meaning model are installed (checked when the page opens).
    ocr_ready: bool,
    model_ready: bool,
    /// Waiting for the new combination for this hotkey.
    recording: Option<crate::shell::Which>,
    message: Option<SharedString>,
    scroll: ScrollHandle,
    pub visible: bool,
}

impl Settings {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Keep the index status fresh while the page is open.
        // While recording a hotkey, every key goes here first (not to the
        // overlay's own keys: Esc mustn't close it, Tab mustn't switch tabs).
        let me = cx.entity().downgrade();
        cx.intercept_keystrokes(move |event, _window, cx| {
            let Some(settings) = me.upgrade() else { return };
            if settings.read(cx).recording.is_none() {
                return;
            }
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            settings.update(cx, |settings, cx| settings.recorded(&keystroke, cx));
        })
        .detach();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = this
                .update(cx, |this, cx| {
                    if this.visible && this.section == Section::Search {
                        this.status = s::get_file_search_status(crate::host::host()).ok();
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();
        Settings {
            section: Section::Search,
            options: None,
            status: None,
            extensions: cx.new(|cx| TextInput::new("e.g. tmp, log", cx)),
            new_app: cx.new(|cx| TextInput::new("App name, e.g. KeePassXC.exe", cx)),
            confirm_clear: false,
            login: startup::enabled(),
            message: None,
            scroll: ScrollHandle::new(),
            visible: false,
            recording: None,
            ocr_ready: false,
            model_ready: false,
        }
    }

    /// Re-reads everything (each time the page opens).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let host = crate::host::host();
        self.options = s::get_file_search_index_options(host.clone()).ok().flatten();
        self.status = s::get_file_search_status(host).ok();
        self.login = startup::enabled();
        self.ocr_ready = dash_search::ocr::ocr_available();
        self.model_ready = dash_search::embedding::model_files_present();
        self.confirm_clear = false;
        self.message = None;
        let extensions = self.options.as_ref().map(|o| o.exclude_extensions.join(", ")).unwrap_or_default();
        self.extensions.update(cx, |input, cx| input.set_text(&extensions, cx));
        cx.notify();
    }

    fn save(&mut self, change: impl FnOnce(&mut FileSearchIndexOptions), needs_rebuild: bool, cx: &mut Context<Self>) {
        let Some(options) = self.options.as_mut() else { return };
        change(options);
        let result = s::save_file_search_index_options(crate::host::host(), options.clone());
        self.message = Some(match result {
            Err(e) => format!("Couldn't save: {e}").into(),
            Ok(()) if needs_rebuild => "Saved. Rebuild the index to apply.".into(),
            Ok(()) => "Saved.".into(),
        });
        cx.notify();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(options) = self.options.clone() else { return };
        let host = crate::host::host();
        crate::host::runtime().spawn(async move {
            let _ = s::start_file_search_index(host, options).await;
        });
        self.message = Some("Rebuilding the index…".into());
        cx.notify();
    }

    fn add_folders(&mut self, which: Folders, window: &mut Window, cx: &mut Context<Self>) {
        PICKING.store(true, Ordering::SeqCst);
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add".into()),
        });
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let picked = picked.await.ok().and_then(|r| r.ok()).flatten().unwrap_or_default();
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            PICKING.store(false, Ordering::SeqCst);
            let _ = this.update(cx, |this, cx| {
                if picked.is_empty() {
                    return;
                }
                this.save(
                    |o| {
                        let list = folders_mut(o, which);
                        for path in picked {
                            let path = path.display().to_string();
                            if !list.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
                                list.push(path);
                            }
                        }
                    },
                    true,
                    cx,
                );
            });
        })
        .detach();
    }

    fn remove_folder(&mut self, which: Folders, index: usize, cx: &mut Context<Self>) {
        self.save(
            |o| {
                let list = folders_mut(o, which);
                if index < list.len() {
                    list.remove(index);
                }
            },
            true,
            cx,
        );
    }

    fn render_search(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(o) = self.options.clone() else {
            return note("The index hasn't been set up yet — it starts on first run.");
        };
        let status_line = match &self.status {
            Some(st) if st.filename_indexing || st.indexing => {
                format!("Indexing… {} names, {} documents so far", st.filename_indexed_files, st.indexed_files)
            }
            Some(st) => format!(
                "{} names · {} documents · watching: {}",
                st.filename_indexed_files, st.indexed_files, st.diagnostics.watcher_strategy
            ),
            None => "Status unavailable".into(),
        };
        let indexing = self.status.as_ref().is_some_and(|st| st.filename_indexing || st.indexing);
        let content_on = o.content_indexing_enabled && o.index_content;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                row("Index", Some(status_line)).child(
                    div()
                        .flex()
                        .gap_2()
                        .child(button("rebuild", "Rebuild now").on_click(cx.listener(|this, _, _, cx| this.rebuild(cx))))
                        .when(indexing, |d| {
                            d.child(button("stop", "Stop").on_click(cx.listener(|this, _, _, cx| {
                                let _ = s::cancel_file_search_index_build();
                                this.message = Some("Stopping…".into());
                                cx.notify();
                            })))
                        }),
                ),
            )
            .child(self.folder_list(
                Folders::Names,
                "Name search covers",
                "Folders or whole drives whose file names you can find.",
                &o.filename_roots,
                cx,
            ))
            .child(row("Search inside files", Some("Index the text of documents and code.".into())).child(toggle(
                "content",
                content_on,
                cx.listener(move |this, _, _, cx| {
                    this.save(
                        |o| {
                            o.content_indexing_enabled = !content_on;
                            o.index_content = !content_on;
                        },
                        true,
                        cx,
                    )
                }),
            )))
            .when(content_on, |d| {
                d.child(self.folder_list(
                    Folders::Inside,
                    "Inside search covers",
                    "Folders whose files' text is indexed.",
                    &o.roots,
                    cx,
                ))
                .child(row("Largest file read", Some("Bigger files are found by name only.".into())).child(chips(
                    "max-kb",
                    &[(256, "256 KB"), (1024, "1 MB"), (4096, "4 MB"), (16384, "16 MB")],
                    o.max_content_kb.unwrap_or(1024),
                    cx,
                    |this, kb, cx| this.save(|o| o.max_content_kb = Some(kb), true, cx),
                )))
            })
            .child(self.folder_list(
                Folders::Skipped,
                "Skipped folders",
                "Never indexed (Windows and Program Files are always skipped).",
                &o.exclude_folders,
                cx,
            ))
            .child(
                row("Skipped file types", Some("Extensions, separated by commas.".into())).child(
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(field(self.extensions.clone()).w(px(240.)))
                        .child(button("save-ext", "Apply").on_click(cx.listener(|this, _, _, cx| {
                            let list: Vec<String> = this
                                .extensions
                                .read(cx)
                                .text()
                                .split(|c: char| c == ',' || c.is_whitespace())
                                .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                                .filter(|e| !e.is_empty())
                                .collect();
                            this.save(|o| o.exclude_extensions = list, true, cx);
                        }))),
                ),
            )
            .when(content_on, |d| {
                let ocr_on = o.ocr_on_index_enabled;
                let semantic_on = o.semantic_search_enabled;
                let (ocr_ready, model_ready) = (self.ocr_ready, self.model_ready);
                d.child(
                    row(
                        "Read text in pictures and scans",
                        Some(if ocr_ready {
                            "OCR for images and scanned PDFs in the folders below. Slower indexing.".into()
                        } else {
                            "OCR isn't installed with this copy of Dash.".into()
                        }),
                    )
                    .child(toggle(
                        "ocr",
                        ocr_on,
                        cx.listener(move |this, _, _, cx| this.save(|o| o.ocr_on_index_enabled = !ocr_on, true, cx)),
                    )),
                )
                .when(ocr_on, |d| {
                    d.child(self.folder_list(
                        Folders::Ocr,
                        "Read pictures in",
                        "Only these folders (OCR is slow on whole drives).",
                        &o.ocr_on_index_folders,
                        cx,
                    ))
                })
                .child(
                    row(
                        "Search by meaning",
                        Some(if model_ready {
                            "Finds documents about what you type, not only exact words. Uses more memory.".into()
                        } else {
                            "The meaning model isn't installed with this copy of Dash.".into()
                        }),
                    )
                    .child(toggle(
                        "semantic",
                        semantic_on,
                        cx.listener(move |this, _, _, cx| {
                            this.save(|o| o.semantic_search_enabled = !semantic_on, true, cx)
                        }),
                    )),
                )
            })
            .child(row("Hidden files", Some("Include hidden files and folders.".into())).child(toggle(
                "hidden",
                o.include_hidden,
                cx.listener(move |this, _, _, cx| this.save(|o| o.include_hidden = !o.include_hidden, true, cx)),
            )))
            .child(row("Watch for changes", Some("Keep the index up to date as files change.".into())).child(toggle(
                "watch",
                o.watcher_enabled,
                cx.listener(move |this, _, _, cx| this.save(|o| o.watcher_enabled = !o.watcher_enabled, false, cx)),
            )))
            .child(row("Indexing speed", Some("Quiet uses the least CPU.".into())).child(chips(
                "speed",
                &[(0, "Quiet"), (1, "Balanced"), (2, "Fast")],
                match o.performance_mode.as_str() {
                    "quiet" => 0,
                    "fast" => 2,
                    _ => 1,
                },
                cx,
                |this, v, cx| {
                    let mode = ["quiet", "balanced", "fast"][v as usize].to_string();
                    this.save(|o| o.performance_mode = mode, false, cx)
                },
            )))
            .into_any_element()
    }

    fn folder_list(
        &self,
        which: Folders,
        title: &'static str,
        hint: &'static str,
        folders: &[String],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = title;
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(row(title, Some(hint.into())).child(button(id, "Add folder…").on_click(cx.listener(
                move |this, _, window, cx| this.add_folders(which, window, cx),
            ))))
            .children(folders.iter().enumerate().map(|(ix, folder)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .text_xs()
                    .text_color(theme::text_2())
                    .child(div().flex_1().truncate().child(folder.clone()))
                    .child(
                        div()
                            .id((id, ix))
                            .px_2()
                            .rounded_md()
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme::text()).bg(theme::panel_2()))
                            .child("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_folder(which, ix, cx))),
                    )
            }))
            .when(folders.is_empty(), |d| d.child(div().pl_3().text_xs().text_color(theme::muted()).child("None")))
    }

    fn render_clipboard(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use dash_clipboard as c;
        let capturing = !c::get_clipboard_paused();
        let images = c::get_clipboard_images_enabled();
        let days = c::get_clipboard_retention_days();
        let image_days = c::get_clipboard_image_retention_days();
        let apps = c::get_clipboard_exclusions();
        let confirm = self.confirm_clear;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(row("Save what I copy", Some("Off pauses the history until you turn it back on.".into())).child(toggle(
                "capture",
                capturing,
                cx.listener(move |_, _, _, cx| {
                    let _ = c::set_clipboard_paused(capturing);
                    cx.notify();
                }),
            )))
            .child(row("Save images", Some("Screenshots and copied pictures, GIFs included.".into())).child(toggle(
                "images",
                images,
                cx.listener(move |_, _, _, cx| {
                    let _ = c::set_clipboard_images_enabled(!images);
                    cx.notify();
                }),
            )))
            .child(row("Keep text for", Some("Pinned items are always kept.".into())).child(chips(
                "days",
                &[(1, "1 day"), (7, "7 days"), (14, "14 days"), (30, "30 days"), (90, "90 days"), (0, "Always")],
                days,
                cx,
                |_, d, cx| {
                    let _ = c::set_clipboard_retention_days(d);
                    cx.notify();
                },
            )))
            .child(row("Keep images for", None).child(chips(
                "image-days",
                &[(1, "1 day"), (7, "7 days"), (14, "14 days"), (30, "30 days"), (90, "90 days"), (0, "Always")],
                image_days,
                cx,
                |_, d, cx| {
                    let _ = c::set_clipboard_image_retention_days(d);
                    cx.notify();
                },
            )))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        row("Never save from", Some("Copies made in these apps are skipped (password managers by default).".into()))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(field(self.new_app.clone()).w(px(220.)))
                                    .child(button("add-app", "Add").on_click(cx.listener(|this, _, _, cx| {
                                        let name = this.new_app.read(cx).text().trim().to_string();
                                        if name.is_empty() {
                                            return;
                                        }
                                        let mut apps = c::get_clipboard_exclusions();
                                        if !apps.iter().any(|a| a.eq_ignore_ascii_case(&name)) {
                                            apps.push(name);
                                            let _ = c::set_clipboard_exclusions(apps);
                                        }
                                        this.new_app.update(cx, |input, cx| input.set_text("", cx));
                                        cx.notify();
                                    })))
                                    .child(button("reset-apps", "Reset").on_click(cx.listener(|_, _, _, cx| {
                                        let _ = c::reset_clipboard_exclusions_to_defaults();
                                        cx.notify();
                                    }))),
                            ),
                    )
                    .children(apps.iter().enumerate().map(|(ix, app)| {
                        let name = app.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl_3()
                            .text_xs()
                            .text_color(theme::text_2())
                            .child(div().flex_1().truncate().child(app.clone()))
                            .child(
                                div()
                                    .id(("app", ix))
                                    .px_2()
                                    .rounded_md()
                                    .text_color(theme::muted())
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme::text()).bg(theme::panel_2()))
                                    .child("Remove")
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let apps: Vec<String> =
                                            c::get_clipboard_exclusions().into_iter().filter(|a| *a != name).collect();
                                        let _ = c::set_clipboard_exclusions(apps);
                                        cx.notify();
                                    })),
                            )
                    })),
            )
            .child(row("Clear history", Some("Removes everything except pinned items.".into())).child(
                button("clear", if confirm { "Click again to clear" } else { "Clear…" }).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if this.confirm_clear {
                            let _ = c::clear_clipboard_history();
                            this.confirm_clear = false;
                            this.message = Some("History cleared.".into());
                        } else {
                            this.confirm_clear = true;
                        }
                        cx.notify();
                    },
                )),
            ))
            .into_any_element()
    }

    fn hotkey_row(
        &self,
        which: crate::shell::Which,
        title: &'static str,
        hint: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let recording = self.recording == Some(which);
        let id = if which == crate::shell::Which::Search { "hk-search" } else { "hk-clipboard" };
        row(title, Some(hint.into())).child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(if recording {
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(theme::accent())
                        .bg(theme::accent_soft())
                        .text_xs()
                        .text_color(theme::text())
                        .child("Press the new keys… (Esc cancels)")
                        .into_any_element()
                } else {
                    keycap(crate::shell::current(which, cx)).into_any_element()
                })
                .when(!recording, |d| {
                    d.child(button(id, "Change").on_click(cx.listener(move |this, _, _, cx| {
                        this.start_recording(which, cx);
                    })))
                }),
        )
    }

    fn start_recording(&mut self, which: crate::shell::Which, cx: &mut Context<Self>) {
        crate::shell::suspend(true, cx);
        self.recording = Some(which);
        self.message = None;
        cx.notify();
    }

    /// Ends recording without a change (and turns the hotkeys back on).
    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.take().is_some() {
            crate::shell::suspend(false, cx);
            cx.notify();
        }
    }

    fn recorded(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some(which) = self.recording else { return };
        let key = keystroke.key.as_str();
        // Ctrl / Alt / Shift / Win on their own: wait for the real key.
        if matches!(key, "control" | "alt" | "shift" | "platform" | "function") {
            return;
        }
        let plain = !(keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform);
        if key == "escape" && plain {
            self.stop_recording(cx);
            return;
        }
        let result = crate::shell::from_keystroke(keystroke).and_then(|hotkey| crate::shell::change(which, hotkey, cx));
        match result {
            Ok(()) => {
                self.message = Some(format!("Saved: {}", crate::shell::current(which, cx)).into());
                self.stop_recording(cx);
            }
            Err(e) => {
                self.message = Some(e.into());
                cx.notify();
            }
        }
    }

    fn render_general(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let login = self.login;
        let data = crate::host::host().data_dir().unwrap_or_default();
        let engines = match dash_core::link::role() {
            dash_core::link::Role::Follower => {
                let app = dash_core::link::owner(&data).map(|o| o.app).unwrap_or_else(|| "the other app".into());
                format!("{app} runs indexing and clipboard saving; Dash uses them and takes over when {app} quits.")
            }
            _ => "Dash runs indexing and clipboard saving here.".into(),
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(row("Indexing and clipboard", Some(engines)))
            .child(self.hotkey_row(crate::shell::Which::Search, "Search", "Opens Dash in Search mode from anywhere.", cx))
            .child(self.hotkey_row(crate::shell::Which::Clipboard, "Clipboard", "Opens the clipboard history.", cx))
            .child(row("Settings", Some("Inside Dash.".into())).child(keycap("Ctrl+,")))
            .child(row(START_AT_LOGIN, Some("Dash waits in the tray after you sign in.".into())).child(toggle(
                "login",
                login,
                cx.listener(move |this, _, _, cx| {
                    match startup::set(!login) {
                        Ok(()) => this.login = startup::enabled(),
                        Err(e) => this.message = Some(format!("Couldn't change it: {e}").into()),
                    }
                    cx.notify();
                }),
            )))
            .child(row("Data folder", Some(data.display().to_string())).child(button("open-data", "Open").on_click(
                move |_, _, _| {
                    let _ = std::process::Command::new("explorer").arg(&data).spawn();
                },
            )))
            .child(row("Quit Dash", Some("Stops indexing and clipboard saving until you start it again.".into())).child(
                button("quit", "Quit").on_click(|_, _, cx| {
                    s::stop_all_index_workers();
                    cx.quit();
                }),
            ))
            .into_any_element()
    }

    fn nav(&self, section: Section, label: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.section == section;
        div()
            .id(label)
            .px_3()
            .py_1p5()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(active, |d| d.bg(theme::accent_soft()).text_color(theme::text()))
            .when(!active, |d| d.text_color(theme::text_2()).hover(|s| s.bg(theme::panel())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.section = section;
                this.confirm_clear = false;
                this.message = None;
                cx.notify();
            }))
            .child(label)
    }
}

impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.section {
            Section::Search => self.render_search(cx),
            Section::Clipboard => self.render_clipboard(cx),
            Section::General => self.render_general(cx),
        };
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w(px(180.))
                    .flex_none()
                    .p_3()
                    .border_r_1()
                    .border_color(theme::border())
                    .child(self.nav(Section::Search, "Search", cx))
                    .child(self.nav(Section::Clipboard, "Clipboard", cx))
                    .child(self.nav(Section::General, "General", cx))
                    .when_some(self.message.clone(), |d, m| {
                        d.child(div().mt_4().px_1().text_xs().text_color(theme::accent()).child(m))
                    }),
            )
            .child(
                div()
                    .id("settings-body")
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px_5()
                    .py_4()
                    .child(body),
            )
    }
}

const START_AT_LOGIN: &str = if cfg!(windows) { "Start with Windows" } else { "Start at login" };

fn folders_mut(o: &mut FileSearchIndexOptions, which: Folders) -> &mut Vec<String> {
    match which {
        Folders::Names => &mut o.filename_roots,
        Folders::Inside => &mut o.roots,
        Folders::Skipped => &mut o.exclude_folders,
        Folders::Ocr => &mut o.ocr_on_index_folders,
    }
}

fn note(text: &'static str) -> gpui::AnyElement {
    div().text_sm().text_color(theme::muted()).child(text).into_any_element()
}

/// A setting: title and hint on the left, the control on the right.
fn row(title: &'static str, hint: Option<String>) -> gpui::Div {
    div().flex().items_center().gap_4().child(
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(div().text_sm().text_color(theme::text()).child(title))
            .when_some(hint, |d, h| d.child(div().text_xs().text_color(theme::muted()).truncate().child(h))),
    )
}

fn button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .text_color(theme::text())
        .bg(theme::panel_2())
        .border_1()
        .border_color(theme::border())
        .cursor_pointer()
        .hover(|s| s.bg(theme::border()))
        .child(label)
}

fn keycap(keys: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_1()
        .border_color(theme::border())
        .bg(theme::panel_2())
        .text_xs()
        .text_color(theme::text_2())
        .child(keys.into())
}

fn field(input: Entity<TextInput>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .px_2()
        .h(px(28.))
        .rounded_md()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::border())
        .text_xs()
        .child(input)
}

fn toggle(
    id: &'static str,
    on: bool,
    listener: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .w(px(36.))
        .h(px(20.))
        .rounded_full()
        .p(px(2.))
        .flex()
        .cursor_pointer()
        .when(on, |d| d.bg(theme::accent()).justify_end())
        .when(!on, |d| d.bg(theme::border()).justify_start())
        .on_click(listener)
        .child(div().w(px(16.)).h(px(16.)).rounded_full().bg(theme::text()))
}

fn chips(
    id: &'static str,
    choices: &[(u32, &'static str)],
    current: u32,
    cx: &mut Context<Settings>,
    pick: fn(&mut Settings, u32, &mut Context<Settings>),
) -> impl IntoElement {
    div().flex().flex_none().gap_1().children(choices.iter().map(|&(value, label)| {
        let active = value == current;
        div()
            .id((id, value as usize))
            .px_2()
            .py_1()
            .rounded_md()
            .text_xs()
            .cursor_pointer()
            .when(active, |d| d.bg(theme::accent_soft()).text_color(theme::text()))
            .when(!active, |d| d.text_color(theme::muted()).hover(|s| s.text_color(theme::text_2())))
            .on_click(cx.listener(move |this, _, _, cx| pick(this, value, cx)))
            .child(label)
    }))
}

/// Start with Windows: a value under the user's Run key.
mod startup {
    #[cfg(windows)]
    use windows::{
        core::{w, HSTRING},
        Win32::System::Registry::*,
    };

    #[cfg(windows)]
    const RUN: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

    #[cfg(windows)]
    pub fn enabled() -> bool {
        unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN, w!("Dash"), RRF_RT_REG_SZ, None, None, None).is_ok() }
    }

    #[cfg(windows)]
    pub fn set(on: bool) -> Result<(), String> {
        unsafe {
            let result = if on {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let value: Vec<u16> =
                    format!("\"{}\" --background", exe.display()).encode_utf16().chain(Some(0)).collect();
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    RUN,
                    &HSTRING::from("Dash"),
                    REG_SZ.0,
                    Some(value.as_ptr().cast()),
                    (value.len() * 2) as u32,
                )
            } else {
                RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, w!("Dash"))
            };
            result.ok().map_err(|e| e.message().to_string())
        }
    }

    /// macOS: a LaunchAgent; Linux: an XDG autostart entry.
    #[cfg(not(windows))]
    fn entry() -> Option<std::path::PathBuf> {
        let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
        Some(if cfg!(target_os = "macos") {
            home.join("Library/LaunchAgents/app.dash.plist")
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("autostart/dash.desktop")
        })
    }

    #[cfg(not(windows))]
    pub fn enabled() -> bool {
        entry().is_some_and(|path| path.is_file())
    }

    #[cfg(not(windows))]
    pub fn set(on: bool) -> Result<(), String> {
        let path = entry().ok_or("No home folder")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe = exe.display().to_string();
        let text = if cfg!(target_os = "macos") {
            [
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#,
                r#"<plist version="1.0"><dict>"#,
                "<key>Label</key><string>app.dash</string>",
                &format!(
                    "<key>ProgramArguments</key><array><string>{}</string><string>--background</string></array>",
                    xml_escape(&exe)
                ),
                "<key>RunAtLoad</key><true/>",
                "</dict></plist>",
                "",
            ]
            .join("\n")
        } else {
            [
                "[Desktop Entry]",
                "Type=Application",
                "Name=Dash",
                "Comment=Search and clipboard history",
                &format!("Exec={} --background", desktop_exec_arg(&exe)),
                "X-GNOME-Autostart-enabled=true",
                "",
            ]
            .join("\n")
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }

    /// Text for a plist `<string>`.
    #[cfg(any(not(windows), test))]
    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
    }

    /// One quoted argument for a .desktop `Exec=` line (Desktop Entry spec):
    /// inside the quotes `"` `` ` `` `$` `\` take a backslash; then the
    /// value's own escaping doubles every backslash, and `%` is `%%`.
    #[cfg(any(not(windows), test))]
    fn desktop_exec_arg(arg: &str) -> String {
        let mut quoted = String::from("\"");
        for ch in arg.chars() {
            if matches!(ch, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(ch);
        }
        quoted.push('"');
        quoted.replace('\\', "\\\\").replace('%', "%%")
    }

    #[cfg(test)]
    mod tests {
        use super::{desktop_exec_arg, xml_escape};

        #[test]
        fn autostart_entries_escape_the_path() {
            assert_eq!(xml_escape("/A&B/<x>"), "/A&amp;B/&lt;x&gt;");
            assert_eq!(desktop_exec_arg("/opt/dash"), "\"/opt/dash\"");
            assert_eq!(desktop_exec_arg("/my apps/da\"sh"), "\"/my apps/da\\\\\"sh\"");
            assert_eq!(desktop_exec_arg("/a$b/100%"), "\"/a\\\\$b/100%%\"");
        }
    }
}
