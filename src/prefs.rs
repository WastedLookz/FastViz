//! Preferences dialog.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::viewer::SlideshowOptions;

/// Which folder opens at startup.
#[derive(Debug, Default)]
pub struct Startup {
    /// `None` means Home.
    pub folder: Option<PathBuf>,
    /// Open the last-viewed folder instead of `folder`.
    pub open_last: bool,
}

/// Shows the dialog over `window`; edits apply to `startup` immediately.
/// `current` is the folder on screen, offered as a one-click start folder.
pub fn present(
    window: &adw::ApplicationWindow,
    startup: &Rc<RefCell<Startup>>,
    current: Option<PathBuf>,
    black: bool,
    on_black: impl Fn(bool) + 'static,
    slideshow: SlideshowOptions,
    on_slideshow: impl Fn(SlideshowOptions) + 'static,
) {
    let state = startup.borrow();
    let folder_row = adw::ActionRow::builder()
        .title("Start Folder")
        .subtitle(folder_label(state.folder.as_deref()))
        // Paths like "Food & Drink" must not be parsed as markup.
        .use_markup(false)
        .sensitive(!state.open_last)
        .build();
    let use_current = gtk::Button::builder()
        .label("Use Current")
        .valign(gtk::Align::Center)
        .sensitive(current.is_some())
        .build();
    let choose = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Choose Folder…")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    folder_row.add_suffix(&use_current);
    folder_row.add_suffix(&choose);

    let open_last = adw::SwitchRow::builder()
        .title("Open Last Folder")
        .subtitle("Start where you left off instead")
        .active(state.open_last)
        .build();
    drop(state);

    use_current.connect_clicked(glib::clone!(
        #[strong]
        startup,
        #[weak]
        folder_row,
        move |_| {
            if let Some(path) = current.clone() {
                set_folder(&startup, &folder_row, path);
            }
        }
    ));
    choose.connect_clicked(glib::clone!(
        #[strong]
        startup,
        #[weak]
        folder_row,
        #[weak]
        window,
        move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Start Folder")
                .modal(true)
                .build();
            if let Some(folder) = startup.borrow().folder.as_ref() {
                dialog.set_initial_folder(Some(&gio::File::for_path(folder)));
            }
            dialog.select_folder(
                Some(&window),
                None::<&gio::Cancellable>,
                glib::clone!(
                    #[strong]
                    startup,
                    #[weak]
                    folder_row,
                    move |result| {
                        if let Some(path) = result.ok().and_then(|file| file.path()) {
                            set_folder(&startup, &folder_row, path);
                        }
                    }
                ),
            );
        }
    ));
    open_last.connect_active_notify(glib::clone!(
        #[strong]
        startup,
        #[weak]
        folder_row,
        move |row| {
            startup.borrow_mut().open_last = row.is_active();
            folder_row.set_sensitive(!row.is_active());
        }
    ));

    let group = adw::PreferencesGroup::builder().title("Startup").build();
    group.add(&folder_row);
    group.add(&open_last);
    let black_row = adw::SwitchRow::builder()
        .title("Black Background")
        .subtitle("Instead of 50% gray; B switches it while viewing")
        .active(black)
        .build();
    black_row.connect_active_notify(move |row| on_black(row.is_active()));
    let viewer_group = adw::PreferencesGroup::builder().title("Fullscreen").build();
    viewer_group.add(&black_row);

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&viewer_group);
    page.add(&slideshow_group(slideshow, on_slideshow));
    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(Some(window));
}

/// Interval, loop and shuffle; each change applies immediately.
fn slideshow_group(
    options: SlideshowOptions,
    on_change: impl Fn(SlideshowOptions) + 'static,
) -> adw::PreferencesGroup {
    let interval = adw::SpinRow::builder()
        .title("Seconds per Photo")
        .subtitle("Videos play to the end; F5 starts a slideshow")
        .adjustment(&gtk::Adjustment::new(
            options.interval,
            1.0,
            60.0,
            0.5,
            1.0,
            0.0,
        ))
        .digits(1)
        .build();
    let repeat = adw::SwitchRow::builder()
        .title("Loop")
        .subtitle("Start over after the last item")
        .active(options.repeat)
        .build();
    let shuffle = adw::SwitchRow::builder()
        .title("Shuffle")
        .subtitle("Random order")
        .active(options.shuffle)
        .build();
    let state = Rc::new(Cell::new(options));
    let on_change = Rc::new(on_change);
    let update = move |change: &dyn Fn(&mut SlideshowOptions)| {
        let mut options = state.get();
        change(&mut options);
        state.set(options);
        on_change(options);
    };
    let update = Rc::new(update);
    interval.connect_value_notify(glib::clone!(
        #[strong]
        update,
        move |row| update(&|o| o.interval = row.value())
    ));
    repeat.connect_active_notify(glib::clone!(
        #[strong]
        update,
        move |row| update(&|o| o.repeat = row.is_active())
    ));
    shuffle.connect_active_notify(move |row| update(&|o| o.shuffle = row.is_active()));

    let group = adw::PreferencesGroup::builder().title("Slideshow").build();
    group.add(&interval);
    group.add(&repeat);
    group.add(&shuffle);
    group
}

fn set_folder(startup: &RefCell<Startup>, row: &adw::ActionRow, path: PathBuf) {
    row.set_subtitle(&folder_label(Some(&path)));
    startup.borrow_mut().folder = Some(path);
}

fn folder_label(folder: Option<&Path>) -> String {
    folder.map_or_else(|| "Home".to_owned(), |p| p.display().to_string())
}
