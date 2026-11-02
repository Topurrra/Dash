//! The first-run screen: what the hotkeys are, how far indexing has got,
//! and a few tips. Shown in Search until it's dismissed (the button, or
//! typing a search); remembered in `preferences\dash-welcome.json`.

use super::*;

fn seen_path() -> PathBuf {
    dash_core::shared::shared_data_dir().join("preferences").join("dash-welcome.json")
}

/// Whether the welcome screen is still to be shown.
pub(super) fn first_run() -> bool {
    !seen_path().exists()
}

/// Index progress for the screen: names, documents, still indexing.
#[derive(Clone, Copy, Default)]
pub(super) struct Progress {
    names: u64,
    documents: u64,
    indexing: bool,
}

impl Overlay {
    pub(super) fn showing_welcome(&self) -> bool {
        self.welcome && self.mode == Mode::Search
    }

    /// Dismissed: not shown again.
    pub(super) fn finish_welcome(&mut self, cx: &mut Context<Self>) {
        if !self.welcome {
            return;
        }
        self.welcome = false;
        let path = seen_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, serde_json::json!({ "seen": true }).to_string());
        cx.notify();
    }

    /// Keeps the progress fresh (once a second) while the screen is up.
    pub(super) fn watch_welcome_progress(&mut self, cx: &mut Context<Self>) {
        if !self.welcome || self.welcome_watching {
            return;
        }
        self.welcome_watching = true;
        cx.spawn(async move |this, cx| loop {
            let progress = crate::host::runtime()
                .spawn_blocking(|| {
                    dash_search::search::get_file_search_status(crate::host::host())
                        .map(|s| Progress {
                            names: s.filename_indexed_files,
                            documents: s.indexed_files,
                            indexing: s.filename_indexing || s.indexing,
                        })
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default();
            let keep = this
                .update(cx, |this, cx| {
                    this.welcome_progress = progress;
                    cx.notify();
                    let keep = this.welcome;
                    if !keep {
                        this.welcome_watching = false;
                    }
                    keep
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(1)).await;
        })
        .detach();
    }

    pub(super) fn render_welcome(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let progress = self.welcome_progress;
        let key = |caps: String, what: &'static str| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_6()
                .child(div().text_sm().text_color(theme::text_2()).child(what))
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme::panel_2())
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme::text())
                        .child(caps),
                )
        };
        let card = |title: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap_2p5()
                .flex_1()
                .p_4()
                .rounded_lg()
                .border_1()
                .border_color(theme::border())
                .bg(theme::panel())
                .child(div().text_xs().font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::muted()).child(title))
        };
        let status = if progress.indexing {
            "Indexing your drives — search works on what's indexed so far."
        } else if progress.names > 0 {
            "Your files are indexed. The index stays fresh as files change."
        } else {
            "Getting the index ready…"
        };
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .w(px(640.))
                    .child(
                        div().flex().items_center().gap_4().child(img(logo_image()).size(px(48.)).flex_none()).child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_size(px(20.))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child("Welcome to Dash"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::muted())
                                        .child("Find any file in a keystroke, and everything you copied."),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(
                                card("Open Dash from anywhere")
                                    .child(key(crate::shell::current(crate::shell::Which::Search, cx), "Search files"))
                                    .child(key(
                                        crate::shell::current(crate::shell::Which::Clipboard, cx),
                                        "Clipboard history",
                                    ))
                                    .child(key("Ctrl+,".into(), "Settings")),
                            )
                            .child(
                                card("Index")
                                    .child(
                                        div()
                                            .flex()
                                            .gap_6()
                                            .child(welcome_stat(
                                                table::thousands(usize::try_from(progress.names).unwrap_or(usize::MAX)),
                                                "file names",
                                            ))
                                            .child(welcome_stat(
                                                table::thousands(
                                                    usize::try_from(progress.documents).unwrap_or(usize::MAX),
                                                ),
                                                "documents",
                                            )),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_start()
                                            .gap_2()
                                            .text_xs()
                                            .text_color(theme::muted())
                                            .child(div().mt(px(4.)).size(px(7.)).flex_none().rounded_full().bg(
                                                if progress.indexing {
                                                    theme::accent()
                                                } else if progress.names > 0 {
                                                    theme::success()
                                                } else {
                                                    theme::muted()
                                                },
                                            ))
                                            .child(div().flex_1().child(status)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .text_sm()
                            .text_color(theme::text_2())
                            .child("•  Type any part of a name — case doesn't matter.")
                            .child("•  Tab searches inside documents; Ctrl+P shows or hides the preview.")
                            .child("•  Right-click a result for rename, move, delete and more."),
                    )
                    .child(
                        div().flex().child(
                            div()
                                .id("welcome-start")
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .bg(theme::accent())
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(gpui::white())
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.88))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.finish_welcome(cx);
                                    window.focus(&this.input.focus_handle(cx));
                                }))
                                .child("Start searching"),
                        ),
                    ),
            )
            .into_any_element()
    }
}

fn welcome_stat(value: String, label: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(div().text_size(px(22.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::text()).child(value))
        .child(div().text_xs().text_color(theme::muted()).child(label))
}

/// The logo (drawn in code) as an image, made once (a new image each
/// redraw would be a new texture each time).
fn logo_image() -> Arc<RenderImage> {
    static LOGO: std::sync::OnceLock<Arc<RenderImage>> = std::sync::OnceLock::new();
    Arc::clone(LOGO.get_or_init(|| {
        const N: u32 = 96;
        let mut rgba = crate::logo::rgba(N);
        // GPUI wants BGRA.
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        let buffer = image::RgbaImage::from_raw(N, N, rgba).expect("logo size");
        Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
    }))
}
