//! Embedded camera preview. QR targets belong to the exact frame being displayed.
use crate::camera_frame::{Frame, QrLink, VideoReader};
use gtk4::{gdk, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

const MAX_AGE: Duration = Duration::from_millis(700);
type OpenLink = Rc<dyn Fn(&str)>;

struct View {
    root: gtk4::Box,
    picture: gtk4::Picture,
    canvas: gtk4::Overlay,
    targets: gtk4::Fixed,
    hint: gtk4::Label,
    links: RefCell<Vec<(QrLink, gtk4::Button)>>,
    frame_size: Cell<(usize, usize)>,
    allocation: Cell<(i32, i32)>,
    last_frame: Rc<Cell<Option<Instant>>>,
    opener: OpenLink,
}

// The same contain transform is used for video and click targets, including letterboxing.
fn target_rect(
    bounds: &[(f64, f64); 4],
    frame: (usize, usize),
    canvas: (i32, i32),
) -> (f64, f64, i32, i32) {
    let scale = (f64::from(canvas.0) / frame.0 as f64).min(f64::from(canvas.1) / frame.1 as f64);
    let left = bounds
        .iter()
        .map(|p| p.0)
        .fold(f64::INFINITY, f64::min)
        .clamp(0.0, frame.0 as f64);
    let top = bounds
        .iter()
        .map(|p| p.1)
        .fold(f64::INFINITY, f64::min)
        .clamp(0.0, frame.1 as f64);
    let right = bounds
        .iter()
        .map(|p| p.0)
        .fold(0.0, f64::max)
        .clamp(left, frame.0 as f64);
    let bottom = bounds
        .iter()
        .map(|p| p.1)
        .fold(0.0, f64::max)
        .clamp(top, frame.1 as f64);
    let x = (f64::from(canvas.0) - frame.0 as f64 * scale) / 2.0 + left * scale;
    let y = (f64::from(canvas.1) - frame.1 as f64 * scale) / 2.0 + top * scale;
    (
        x,
        y,
        ((right - left) * scale).round().max(1.0) as i32,
        ((bottom - top) * scale).round().max(1.0) as i32,
    )
}

impl View {
    fn clear(&self, hint: &str) {
        self.picture.set_paintable(None::<&gdk::Paintable>);
        for (_, button) in self.links.borrow_mut().drain(..) {
            self.targets.remove(&button);
        }
        self.last_frame.set(None);
        self.hint.set_text(hint);
    }

    fn layout(&self) {
        let size = (self.canvas.width(), self.canvas.height());
        self.allocation.set(size);
        let frame = self.frame_size.get();
        if frame.0 == 0 || frame.1 == 0 || size.0 <= 0 || size.1 <= 0 {
            return;
        }
        for (link, button) in self.links.borrow().iter() {
            let (x, y, w, h) = target_rect(&link.bounds, frame, size);
            button.set_size_request(w, h);
            self.targets.move_(button, x, y);
        }
    }

    fn render(&self, frame: Frame, timestamp: Instant) {
        self.last_frame.set(Some(timestamp));
        self.frame_size.set((frame.width, frame.height));
        let texture = gdk::MemoryTexture::new(
            frame.width as i32,
            frame.height as i32,
            gdk::MemoryFormat::R8g8b8,
            &glib::Bytes::from_owned(frame.rgb),
            frame.width * 3,
        );
        self.picture.set_paintable(Some(&texture));
        let mut previous = self.links.take();
        let mut current = Vec::new();
        for link in frame.links {
            let button =
                if let Some(index) = previous.iter().position(|(old, _)| old.url == link.url) {
                    previous.remove(index).1
                } else {
                    let button = gtk4::Button::builder()
                        .css_classes(["qr-target"])
                        .tooltip_text(&link.url)
                        .build();
                    button.set_cursor_from_name(Some("pointer"));
                    button.update_property(&[gtk4::accessible::Property::Label(&format!(
                        "Open {}",
                        link.url
                    ))]);
                    let opener = self.opener.clone();
                    let url = link.url.clone();
                    let latest = self.last_frame.clone();
                    button.connect_clicked(move |button| {
                        if button.parent().is_some()
                            && latest.get().is_some_and(|time| time.elapsed() <= MAX_AGE)
                        {
                            opener(&url);
                        }
                    });
                    self.targets.put(&button, 0.0, 0.0);
                    button
                };
            current.push((link, button));
        }
        for (_, button) in previous {
            self.targets.remove(&button);
        }
        self.hint.set_text(if current.is_empty() {
            "Point at a QR link. Click its highlight to open it."
        } else {
            "QR link detected. Hover to inspect, click to open."
        });
        self.links.replace(current);
        self.layout();
    }
}

pub struct CameraPreview {
    view: Rc<View>,
    generation: Arc<AtomicU64>,
    selection: Cell<(Option<u32>, bool)>,
    tx: mpsc::SyncSender<(u64, Instant, Result<Frame, String>)>,
    timer: Option<glib::SourceId>,
}

impl CameraPreview {
    pub fn new() -> Self {
        Self::with_opener(None)
    }

    fn with_opener(opener: Option<OpenLink>) -> Self {
        let picture = gtk4::Picture::builder()
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        picture.set_keep_aspect_ratio(true);
        let canvas = gtk4::Overlay::builder()
            .child(&picture)
            .height_request(280)
            .hexpand(true)
            .overflow(gtk4::Overflow::Hidden)
            .css_classes(["camera-preview"])
            .build();
        let targets = gtk4::Fixed::new();
        canvas.add_overlay(&targets);
        let hint = gtk4::Label::builder()
            .label("Start the camera to scan QR links.")
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        root.append(&canvas);
        root.append(&hint);
        root.set_visible(false);
        let default_hint = hint.downgrade();
        let opener = opener.unwrap_or_else(|| {
            Rc::new(move |url| {
                let hint = default_hint.clone();
                gio::AppInfo::launch_default_for_uri_async(
                    url,
                    None::<&gio::AppLaunchContext>,
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let Err(error) = result {
                            if let Some(label) = hint.upgrade() {
                                label.set_text(&format!("Could not open link: {error}"));
                            }
                        }
                    },
                );
            })
        });
        let css = gtk4::CssProvider::new();
        css.load_from_data(".camera-preview { background: #111; border-radius: 12px; } .qr-target { background: alpha(@accent_bg_color, 0.10); border: 2px solid @accent_color; border-radius: 4px; padding: 0; min-width: 0; min-height: 0; box-shadow: none; } .qr-target:hover, .qr-target:focus-visible { background: alpha(@accent_bg_color, 0.25); border-width: 3px; }");
        if let Some(display) = gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let view = Rc::new(View {
            root,
            picture,
            canvas,
            targets,
            hint,
            opener,
            links: RefCell::new(vec![]),
            frame_size: Cell::new((0, 0)),
            allocation: Cell::new((0, 0)),
            last_frame: Rc::new(Cell::new(None)),
        });
        let generation = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::sync_channel::<(u64, Instant, Result<Frame, String>)>(1);
        let timer = glib::timeout_add_local(Duration::from_millis(33), {
            let weak = Rc::downgrade(&view);
            let generation = generation.clone();
            move || {
                let Some(view) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                while let Ok((epoch, timestamp, result)) = rx.try_recv() {
                    if generation.load(Ordering::Relaxed) != epoch || timestamp.elapsed() > MAX_AGE
                    {
                        continue;
                    }
                    match result {
                        Ok(frame) => view.render(frame, timestamp),
                        Err(error) => view.clear(&error),
                    }
                }
                if view
                    .last_frame
                    .get()
                    .is_some_and(|time| time.elapsed() > MAX_AGE)
                {
                    view.clear("Waiting for camera frames...");
                }
                if view.allocation.get() != (view.canvas.width(), view.canvas.height()) {
                    view.layout();
                }
                glib::ControlFlow::Continue
            }
        });
        Self {
            view,
            generation,
            selection: Cell::new((None, false)),
            tx,
            timer: Some(timer),
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.view.root
    }

    pub fn set_session(&self, pid: Option<u32>, enabled: bool) {
        let key = (pid, enabled);
        if self.selection.replace(key) == key {
            return;
        }
        let epoch = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.view.root.set_visible(enabled && pid.is_some());
        self.view.clear("Waiting for camera frames...");
        if !enabled || pid.is_none() {
            return;
        }
        let generation = self.generation.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            while generation.load(Ordering::Relaxed) == epoch {
                let result = VideoReader::open(crate::devices::VIDEO_DEV);
                match result {
                    Err(error) => {
                        let _ = tx.try_send((epoch, Instant::now(), Err(error)));
                    }
                    Ok(mut reader) => {
                        while generation.load(Ordering::Relaxed) == epoch {
                            let timestamp = Instant::now();
                            match reader.next_frame() {
                                Ok(Some(frame)) => {
                                    let _ = tx.try_send((epoch, timestamp, Ok(frame)));
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    let _ = tx.try_send((epoch, timestamp, Err(error)));
                                    break;
                                }
                            }
                            // Bound preview/decoder work to 15 fps without touching capture FPS.
                            let pause =
                                Duration::from_millis(66).saturating_sub(timestamp.elapsed());
                            std::thread::sleep(pause);
                        }
                    }
                }
                for _ in 0..10 {
                    if generation.load(Ordering::Relaxed) != epoch {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
    }
}

impl Drop for CameraPreview {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn maps_landscape_portrait_and_letterboxed_targets() {
        let bounds = [(100., 50.), (200., 50.), (200., 150.), (100., 150.)];
        assert_eq!(
            target_rect(&bounds, (400, 200), (800, 400)),
            (200., 100., 200, 200)
        );
        assert_eq!(
            target_rect(&bounds, (400, 200), (400, 400)),
            (100., 150., 100, 100)
        );
        assert_eq!(
            target_rect(&bounds, (400, 200), (800, 200)),
            (300., 50., 100, 100)
        );
    }

    pub(crate) fn exercise_clickable_preview(app: &libadwaita::Application) {
        let opened = Rc::new(RefCell::new(Vec::new()));
        let preview = CameraPreview::with_opener(Some({
            let opened = opened.clone();
            Rc::new(move |url| opened.borrow_mut().push(url.to_owned()))
        }));
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .default_width(640)
            .default_height(400)
            .child(preview.widget())
            .build();
        preview.widget().set_visible(true);
        window.present();
        let context = glib::MainContext::default();
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        preview
            .view
            .render(crate::camera_frame::tests::qr_frame(), Instant::now());
        let until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < until {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(preview.view.links.borrow().len(), 1);
        let link = preview.view.links.borrow()[0].0.clone();
        let (x, y, w, h) = target_rect(
            &link.bounds,
            preview.view.frame_size.get(),
            (preview.view.canvas.width(), preview.view.canvas.height()),
        );
        let picked = preview
            .view
            .canvas
            .pick(
                x + f64::from(w) / 2.0,
                y + f64::from(h) / 2.0,
                gtk4::PickFlags::DEFAULT,
            )
            .expect("QR must be clickable at its displayed location");
        let button = preview.view.links.borrow()[0].1.clone();
        assert_eq!(
            button.tooltip_text().as_deref(),
            Some("https://example.com/scan?a=1&b=2")
        );
        assert!(
            picked == button
                || picked.ancestor(gtk4::Button::static_type()) == Some(button.clone().upcast())
        );
        if let Ok(path) = std::env::var("AVYX_QR_SCREENSHOT") {
            let paintable = gtk4::WidgetPaintable::new(Some(&window));
            let until = Instant::now() + Duration::from_millis(150);
            while Instant::now() < until {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let snapshot = gtk4::Snapshot::new();
            paintable.snapshot(
                &snapshot,
                f64::from(window.width()),
                f64::from(window.height()),
            );
            window
                .renderer()
                .unwrap()
                .render_texture(snapshot.to_node().unwrap(), None)
                .save_to_png(path)
                .unwrap();
        }
        button.emit_clicked();
        assert_eq!(
            opened.borrow().as_slice(),
            ["https://example.com/scan?a=1&b=2"]
        );
        // A moving QR retains its keyboard-focusable target and receives new coordinates.
        let mut moved = crate::camera_frame::tests::qr_frame();
        for point in &mut moved.links[0].bounds {
            point.0 += 20.;
        }
        preview.view.render(moved, Instant::now());
        assert_eq!(preview.view.links.borrow()[0].1, button);
        preview.view.render(
            crate::camera_frame::scan_rgb(640, 320, vec![255; 640 * 320 * 3]).unwrap(),
            Instant::now(),
        );
        assert!(preview.view.links.borrow().is_empty());
        button.emit_clicked();
        assert_eq!(
            opened.borrow().len(),
            1,
            "Removed QR targets must not launch links"
        );
        preview.view.render(
            crate::camera_frame::tests::qr_frame(),
            Instant::now() - MAX_AGE - Duration::from_millis(1),
        );
        preview.view.links.borrow()[0].1.emit_clicked();
        assert_eq!(
            opened.borrow().len(),
            1,
            "Expired frames must not launch links"
        );
        window.close();
    }
}
