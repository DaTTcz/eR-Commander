//! er-drag - malý pomocník eR Commanderu pro přetažení souborů do JINÉ
//! aplikace (např. do drag & drop boxu FFMPEG Master, Dolphinu, prohlížeče).
//!
//! Proč zvlášť: eR Commander stojí na egui/winit, které umí soubory do
//! okna jen PŘIJÍMAT, ale neumí tažení ZAČÍT směrem ven (hlavně na
//! Waylandu). GTK4 to umí nativně na Waylandu i X11, takže eR Commander
//! při vytažení souborů z okna spustí tohle malé okénko:
//!
//!     er-drag /cesta/k/souboru1 /cesta/k/souboru2 ...
//!
//! Okénko ukáže, co se přetahuje; chytneš ho myší a přetáhneš do cílové
//! aplikace. Po úspěšném puštění se samo zavře, Esc ho zavře taky.
//! Nabízí se jako `text/uri-list` (file:// URI) - to přijímá prakticky
//! každá aplikace (GTK, Qt/KDE, Tk/tkinterdnd2, prohlížeče).

#[cfg(target_os = "linux")]
mod app {
    use gtk4 as gtk;
    use gtk::prelude::*;
    use gtk::{gdk, gio, glib};
    use std::cell::Cell;
    use std::rc::Rc;

    pub fn run() -> glib::ExitCode {
        let files: Vec<String> = std::env::args().skip(1).collect();
        if files.is_empty() {
            eprintln!("Použití: er-drag <soubor> [soubor...]");
            return glib::ExitCode::FAILURE;
        }

        let app = gtk::Application::builder()
            .application_id("cz.datt.ercommander.drag")
            // Každé spuštění samostatně (bez D-Bus "jediné instance").
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.connect_activate(move |app| build_window(app, &files));
        // Cesty k souborům GTK NEpředáváme - jinak by je bral jako
        // "otevři tyto soubory" a čekal na signál `open`.
        app.run_with_args(&["er-drag"])
    }

    fn build_window(app: &gtk::Application, files: &[String]) {
        let count = files.len();
        let caption = if count == 1 {
            std::path::Path::new(&files[0])
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| files[0].clone())
        } else {
            format!("{} položek", count)
        };

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("eR Commander – přetáhni")
            .default_width(340)
            .resizable(false)
            .build();

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
        vbox.set_margin_top(18);
        vbox.set_margin_bottom(18);
        vbox.set_margin_start(24);
        vbox.set_margin_end(24);

        let all_dirs = files.iter().all(|f| std::path::Path::new(f).is_dir());
        let icon_name = if count > 1 {
            "edit-copy"
        } else if all_dirs {
            "folder"
        } else {
            "text-x-generic"
        };
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.set_pixel_size(64);

        let title = gtk::Label::new(Some(&caption));
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        title.set_max_width_chars(40);

        let hint = gtk::Label::new(Some("Chyť a přetáhni do cílové aplikace\nEsc = zavřít"));
        hint.set_justify(gtk::Justification::Center);
        hint.add_css_class("dim-label");

        vbox.append(&icon);
        vbox.append(&title);
        vbox.append(&hint);

        // file:// URI, řádky oddělené CRLF (tak to text/uri-list chce).
        let uri_list: String = files
            .iter()
            .map(|p| format!("{}\r\n", gio::File::for_path(p).uri()))
            .collect();
        let provider = gdk::ContentProvider::for_bytes(
            "text/uri-list",
            &glib::Bytes::from_owned(uri_list.into_bytes()),
        );

        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::COPY);
        drag.connect_prepare(move |_src, _x, _y| Some(provider.clone()));

        // Zavřít po úspěšném puštění, ale ne po zrušeném tažení (Esc,
        // puštění mimo cíl) - to si uživatel může zkusit znovu.
        let cancelled = Rc::new(Cell::new(false));
        {
            let cancelled = cancelled.clone();
            drag.connect_drag_cancel(move |_src, _drag, _reason| {
                cancelled.set(true);
                false
            });
        }
        {
            let app = app.clone();
            drag.connect_drag_end(move |_src, _drag, _delete| {
                if cancelled.get() {
                    cancelled.set(false);
                } else {
                    app.quit();
                }
            });
        }
        vbox.add_controller(drag);

        let keys = gtk::EventControllerKey::new();
        {
            let window = window.clone();
            keys.connect_key_pressed(move |_ctrl, key, _code, _mods| {
                if key == gdk::Key::Escape {
                    window.close();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        window.add_controller(keys);

        window.set_child(Some(&vbox));
        window.present();
    }
}

#[cfg(target_os = "linux")]
fn main() -> gtk4::glib::ExitCode {
    app::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("er-drag je jen pro Linux - na Windows eR Commander táhne soubory sám.");
}
