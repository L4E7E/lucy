//! Album artwork is fetched off the GTK thread, bounded and never cached on disk.
use gtk::{gdk_pixbuf, gio, prelude::*};
use std::{cell::RefCell, fs, io::Read, process::Command, rc::Rc, sync::mpsc, thread};

const LIMIT: u64 = 2 * 1024 * 1024;

fn load(url: &str) -> Option<Vec<u8>> {
    if url.starts_with("file://") {
        let (path, _) = glib::filename_from_uri(url).ok()?;
        let file = fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > LIMIT {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes).ok()?;
        (bytes.len() as u64 <= LIMIT).then_some(bytes)
    } else if url.starts_with("https://") || url.starts_with("http://") {
        let output = Command::new("curl")
            .args([
                "--silent",
                "--fail",
                "--location",
                "--max-redirs",
                "3",
                "--max-time",
                "3",
                "--max-filesize",
                "2097152",
                "--proto",
                "=http,https",
                "--proto-redir",
                "=http,https",
                "--",
                url,
            ])
            .output()
            .ok()?;
        (output.status.success() && output.stdout.len() as u64 <= LIMIT).then_some(output.stdout)
    } else {
        None
    }
}

pub struct Artwork {
    pub widget: gtk::Image,
    current: Rc<RefCell<String>>,
    tx: mpsc::Sender<Option<String>>,
    source: glib::SourceId,
    worker: Option<thread::JoinHandle<()>>,
}
impl Artwork {
    #[allow(deprecated)]
    pub fn new() -> Self {
        let widget =
            gtk::Image::from_icon_name(Some("audio-x-generic-symbolic"), gtk::IconSize::Dialog);
        widget.set_pixel_size(64);
        widget.set_size_request(80, 80);
        widget.style_context().add_class("album-art");
        let current = Rc::new(RefCell::new(String::new()));
        let (tx, requests) = mpsc::channel::<Option<String>>();
        let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let worker = thread::spawn(move || {
            while let Ok(mut next) = requests.recv() {
                for newer in requests.try_iter() {
                    next = newer;
                }
                let Some(url) = next else {
                    break;
                };
                let bytes = load(&url);
                if events.send((url, bytes)).is_err() {
                    break;
                }
            }
        });
        let weak = widget.downgrade();
        let selected = current.clone();
        let source = receiver.attach(None, move |(url, bytes): (String, Option<Vec<u8>>)| {
            let Some(widget) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if *selected.borrow() == url {
                if let Some(pixbuf) = bytes.and_then(|bytes| {
                    let stream =
                        gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
                    gdk_pixbuf::Pixbuf::from_stream_at_scale(
                        &stream,
                        72,
                        72,
                        true,
                        gio::Cancellable::NONE,
                    )
                    .ok()
                }) {
                    widget.set_from_pixbuf(Some(&pixbuf));
                }
            }
            glib::ControlFlow::Continue
        });
        Self {
            widget,
            current,
            tx,
            source,
            worker: Some(worker),
        }
    }
    pub fn set_url(&self, url: &str) {
        if *self.current.borrow() == url {
            return;
        }
        *self.current.borrow_mut() = url.into();
        self.widget
            .set_from_icon_name(Some("audio-x-generic-symbolic"), gtk::IconSize::Dialog);
        self.widget.set_pixel_size(64);
        if !url.is_empty() {
            let _ = self.tx.send(Some(url.into()));
        }
    }
}
impl Drop for Artwork {
    fn drop(&mut self) {
        let _ = self.tx.send(None);
        if let Some(source) = glib::MainContext::default().find_source_by_id(&self.source) {
            source.destroy();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artwork_rejects_unsupported_sources() {
        assert!(load("data:image/png;base64,bad").is_none());
        assert!(load("file:///not/a/lucy-artwork-file").is_none());
        assert!(load("ftp://example.invalid/art").is_none());
    }
}
