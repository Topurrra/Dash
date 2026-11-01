//! How results are shown, chosen in the header and remembered per mode:
//! Search (Files tab) as Table, List, Grid or Compact; Clipboard as List,
//! Grid or Compact. Also the grid's arithmetic (columns, arrow keys,
//! scrolling) and the small picture thumbnails the Files grid shows.

use std::collections::{HashMap, VecDeque};

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchView {
    Table,
    List,
    Grid,
    Compact,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ClipView {
    List,
    Grid,
    Compact,
}

impl SearchView {
    const ALL: [SearchView; 4] = [SearchView::Table, SearchView::List, SearchView::Grid, SearchView::Compact];

    fn key(self) -> &'static str {
        match self {
            SearchView::Table => "table",
            SearchView::List => "list",
            SearchView::Grid => "grid",
            SearchView::Compact => "compact",
        }
    }

    fn icon(self) -> Icon {
        match self {
            SearchView::Table => Icon::Table,
            SearchView::List => Icon::List,
            SearchView::Grid => Icon::Grid,
            SearchView::Compact => Icon::Rows,
        }
    }
}

impl ClipView {
    const ALL: [ClipView; 3] = [ClipView::List, ClipView::Grid, ClipView::Compact];

    fn key(self) -> &'static str {
        match self {
            ClipView::List => "list",
            ClipView::Grid => "grid",
            ClipView::Compact => "compact",
        }
    }

    fn icon(self) -> Icon {
        match self {
            ClipView::List => Icon::List,
            ClipView::Grid => Icon::Grid,
            ClipView::Compact => Icon::Rows,
        }
    }
}

// ── Remembered between runs ────────────────────────────────────────────

/// Preview pane on / off and each mode's view, kept with the other preferences.
fn prefs_path() -> PathBuf {
    dash_core::shared::shared_data_dir().join("preferences").join("dash-view.json")
}

pub(super) struct ViewPrefs {
    pub show_pane: bool,
    pub search: SearchView,
    pub clipboard: ClipView,
}

pub(super) fn load_prefs() -> ViewPrefs {
    let value = std::fs::read(prefs_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_default();
    let text = |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    ViewPrefs {
        show_pane: value.get("preview").and_then(|p| p.as_bool()).unwrap_or(true),
        search: SearchView::ALL.into_iter().find(|v| v.key() == text("search_view")).unwrap_or(SearchView::Table),
        clipboard: ClipView::ALL.into_iter().find(|v| v.key() == text("clipboard_view")).unwrap_or(ClipView::List),
    }
}

impl Overlay {
    pub(super) fn save_prefs(&self) {
        let path = prefs_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let value = serde_json::json!({
            "preview": self.show_pane,
            "search_view": self.search_view.key(),
            "clipboard_view": self.clip_view.key(),
        });
        let _ = std::fs::write(path, value.to_string());
    }

    /// The header's view switch for the current mode (none in Settings).
    pub(super) fn render_view_switch(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let chip = |id: &'static str, glyph: Icon, active: bool| {
            let color = if active { theme::text() } else { theme::muted() };
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(28.))
                .rounded_md()
                .cursor_pointer()
                .when(active, |d| d.bg(theme::selected()))
                .when(!active, |d| d.hover(|s| s.bg(theme::hover())))
                .child(icon(glyph, 15., color))
        };
        let group = div().flex().flex_none().items_center().gap_0p5().p(px(3.)).rounded_lg().bg(theme::panel());
        match self.mode {
            Mode::Search => Some(
                group
                    .children(SearchView::ALL.into_iter().map(|view| {
                        chip(view.key(), view.icon(), self.search_view == view).on_click(cx.listener(move |this, _, _, cx| {
                            this.set_search_view(view, cx);
                        }))
                    }))
                    .into_any_element(),
            ),
            Mode::Clipboard => Some(
                group
                    .children(ClipView::ALL.into_iter().map(|view| {
                        chip(view.key(), view.icon(), self.clip_view == view).on_click(cx.listener(move |this, _, _, cx| {
                            this.set_clip_view(view, cx);
                        }))
                    }))
                    .into_any_element(),
            ),
            Mode::Settings => None,
        }
    }

    pub(super) fn set_search_view(&mut self, view: SearchView, cx: &mut Context<Self>) {
        self.search_view = view;
        self.save_prefs();
        self.scroll_to_index(self.table.focus_ix(), cx);
        cx.notify();
    }

    pub(super) fn set_clip_view(&mut self, view: ClipView, cx: &mut Context<Self>) {
        self.clip_view = view;
        self.save_prefs();
        self.scroll_to_index(self.selected, cx);
        cx.notify();
    }

    // ── Grids ──────────────────────────────────────────────────────────

    /// The list being shown is a grid (Files tab or Clipboard).
    pub(super) fn grid_active(&self) -> bool {
        (self.files_tab() && self.search_view == SearchView::Grid) || (self.mode == Mode::Clipboard && self.clip_view == ClipView::Grid)
    }

    /// Items per grid row: as many tiles as fit the list's width.
    pub(super) fn grid_cols(&self) -> usize {
        let width = if self.show_pane { if self.files_tab() { 660. } else { 470. } } else { WINDOW_WIDTH };
        let tile = if self.files_tab() { TILE_W } else { CARD_W } + GRID_GAP;
        (((width - 16.) / tile).floor() as usize).max(1)
    }

    /// How far Up / Down move: a row of tiles in a grid.
    pub(super) fn step(&self) -> isize {
        if self.grid_active() { self.grid_cols() as isize } else { 1 }
    }

    /// Scrolls item `ix` into view (its row, in a grid).
    pub(super) fn scroll_to_index(&self, ix: usize, _: &mut Context<Self>) {
        let item = if self.grid_active() { ix / self.grid_cols() } else { ix };
        self.scroll.scroll_to_item(item, ScrollStrategy::Center);
    }

    pub(super) fn on_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.grid_active() {
            self.select(-1, cx);
        }
    }

    pub(super) fn on_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.grid_active() {
            self.select(1, cx);
        }
    }

    // ── Thumbnails ─────────────────────────────────────────────────────

    /// A small picture of an image file, made in the background (`None`
    /// meanwhile, and for files that aren't pictures).
    pub(super) fn thumb(&mut self, path: &str, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        if !is_thumbable(path) {
            return None;
        }
        if let Some(found) = self.thumbs.map.get(path) {
            return found.clone();
        }
        self.thumbs.map.insert(path.to_string(), None);
        let wanted = path.to_string();
        cx.spawn(async move |this, cx| {
            let key = wanted.clone();
            let made = crate::host::runtime().spawn_blocking(move || make_thumb(&wanted)).await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                this.thumbs.add(key, made);
                cx.notify();
            });
        })
        .detach();
        None
    }

    // ── Clipboard: Compact and Grid ────────────────────────────────────

    pub(super) fn render_clips_compact(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.selected;
        let list = uniform_list(
            "clips-compact",
            self.clips.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| this.clips.get(ix).cloned().map(|clip| (ix, clip)))
                    .map(|(ix, clip)| {
                        let active = ix == selected;
                        let age = clip.meta.rsplit(" · ").next().unwrap_or_default().to_string();
                        div()
                            .id(ix)
                            .flex()
                            .items_center()
                            .gap_3()
                            .w_full()
                            .h(px(28.))
                            .px_3()
                            .rounded_md()
                            .cursor_pointer()
                            .when(active, |d| d.bg(theme::selected()))
                            .when(!active, |d| d.hover(|s| s.bg(theme::hover())))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                this.click_clip(ix, event.click_count, window, cx)
                            }))
                            .when(clip.image.is_some(), |d| d.child(icon(Icon::Image, 13., theme::muted())))
                            .child(div().flex_1().min_w_0().text_sm().text_color(theme::text()).one_line().child(clip.text.clone()))
                            .when(clip.pinned, |d| d.child(icon(Icon::Pin, 12., theme::accent())))
                            .child(div().flex_none().text_xs().text_color(theme::muted()).child(age))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .px_2()
        .into_any_element();
        self.with_list_scrollbar(list)
    }

    pub(super) fn render_clips_grid(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.selected;
        let cols = self.grid_cols();
        let rows = self.clips.len().div_ceil(cols);
        let list = uniform_list(
            "clips-grid",
            rows,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|r| {
                        let cards = (r * cols..((r + 1) * cols).min(this.clips.len())).filter_map(|ix| {
                            let clip = this.clips.get(ix).cloned()?;
                            let active = ix == selected;
                            let app_icon = clip.app_path.as_deref().and_then(|p| this.icon(p, false));
                            let body = match clip.image.clone() {
                                Some(path) => div()
                                    .flex_1()
                                    .min_h_0()
                                    .rounded_md()
                                    .overflow_hidden()
                                    .bg(theme::panel_2())
                                    .child(
                                        img(path)
                                            .id(SharedString::from(format!("clip-card-{}", clip.id)))
                                            .size_full()
                                            .object_fit(ObjectFit::Cover),
                                    )
                                    .into_any_element(),
                                None => div()
                                    .flex_1()
                                    .min_h_0()
                                    .text_xs()
                                    .line_height(px(17.))
                                    .text_color(theme::text_2())
                                    .line_clamp(6)
                                    .child(clip.full_text.chars().take(600).collect::<String>())
                                    .into_any_element(),
                            };
                            Some(
                                div()
                                    .id(ix)
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .w(px(CARD_W))
                                    .h(px(CARD_H))
                                    .p_3()
                                    .rounded_lg()
                                    .border_1()
                                    .cursor_pointer()
                                    .when(active, |d| d.bg(theme::selected()).border_color(theme::accent()))
                                    .when(!active, |d| d.bg(theme::panel()).border_color(theme::border()).hover(|s| s.bg(theme::hover())))
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        this.click_clip(ix, event.click_count, window, cx)
                                    }))
                                    .child(body)
                                    .child(
                                        div()
                                            .flex()
                                            .flex_none()
                                            .items_center()
                                            .gap_1p5()
                                            .text_xs()
                                            .text_color(theme::muted())
                                            .when_some(app_icon, |d, app| d.child(img(app).size(px(14.))))
                                            .child(div().flex_1().min_w_0().one_line().child(clip.meta.clone()))
                                            .when(clip.pinned, |d| d.child(icon(Icon::Pin, 12., theme::accent()))),
                                    ),
                            )
                        });
                        div().id(("clip-row", r)).flex().gap(px(GRID_GAP)).h(px(CARD_H + GRID_GAP)).children(cards)
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .px_2()
        .into_any_element();
        self.with_list_scrollbar(list)
    }

    /// A click on a clipboard entry (any view): select it; double: paste.
    pub(super) fn click_clip(&mut self, ix: usize, clicks: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = ix;
        self.update_preview(cx);
        cx.notify();
        if clicks >= 2 {
            self.on_open(&OpenSelected, window, cx);
        }
    }
}

const WINDOW_WIDTH: f32 = 1120.;
/// Files grid tiles.
pub(super) const TILE_W: f32 = 120.;
pub(super) const TILE_H: f32 = 136.;
/// Clipboard grid cards.
const CARD_W: f32 = 212.;
const CARD_H: f32 = 156.;
pub(super) const GRID_GAP: f32 = 8.;

/// The thumbnails kept, oldest dropped first (their textures are freed
/// on the next draw).
#[derive(Default)]
pub(super) struct Thumbs {
    map: HashMap<String, Option<Arc<RenderImage>>>,
    order: VecDeque<String>,
    pub(super) evicted: Vec<Arc<RenderImage>>,
}

const MAX_THUMBS: usize = 300;

impl Thumbs {
    fn add(&mut self, path: String, thumb: Option<Arc<RenderImage>>) {
        self.map.insert(path.clone(), thumb);
        self.order.push_back(path);
        while self.order.len() > MAX_THUMBS {
            if let Some(old) = self.order.pop_front() {
                if let Some(Some(image)) = self.map.remove(&old) {
                    self.evicted.push(image);
                }
            }
        }
    }
}

fn is_thumbable(path: &str) -> bool {
    let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "tif" | "tiff" | "ico")
}

/// Pictures bigger than this get an icon, not a thumbnail.
const MAX_THUMB_SOURCE: u64 = 40 * 1024 * 1024;

/// Decodes `path` (size-capped, in a catch so a broken file can't take the
/// app down) and shrinks it to at most 256 px.
fn make_thumb(path: &str) -> Option<Arc<RenderImage>> {
    if std::fs::metadata(path).ok()?.len() > MAX_THUMB_SOURCE {
        return None;
    }
    let path = path.to_string();
    std::panic::catch_unwind(move || {
        let mut reader = image::ImageReader::open(&path).ok()?.with_guessed_format().ok()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(20_000);
        limits.max_image_height = Some(20_000);
        limits.max_alloc = Some(512 * 1024 * 1024);
        reader.limits(limits);
        let mut small = reader.decode().ok()?.thumbnail(256, 256).to_rgba8();
        // GPUI wants BGRA.
        for px in small.pixels_mut() {
            px.0.swap(0, 2);
        }
        Some(Arc::new(RenderImage::new(vec![image::Frame::new(small)])))
    })
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_are_small_and_capped() {
        let dir = std::env::temp_dir().join(format!("dash-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.png");
        image::RgbaImage::from_pixel(1200, 600, image::Rgba([10, 20, 30, 255])).save(&file).unwrap();
        let thumb = make_thumb(&file.display().to_string()).expect("thumbnail");
        let size = thumb.size(0);
        assert_eq!((size.width.0, size.height.0), (256, 128));
        // Not a picture inside, whatever the name says.
        let fake = dir.join("fake.png");
        std::fs::write(&fake, b"not an image").unwrap();
        assert!(make_thumb(&fake.display().to_string()).is_none());
        let _ = std::fs::remove_dir_all(&dir);

        let mut thumbs = Thumbs::default();
        for i in 0..MAX_THUMBS + 5 {
            thumbs.add(format!("p{i}"), None);
        }
        assert_eq!(thumbs.map.len(), MAX_THUMBS);
        assert!(!thumbs.map.contains_key("p0"));
    }
}
