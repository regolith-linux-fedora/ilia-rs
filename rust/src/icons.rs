use std::path::Path;

use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

// The reference's IconLoader for the later pages. A themed icon that is
// found but fails to load ends the attempt: the reference reports it from an
// outer catch and returns no icon, without trying further fallbacks.

/// Loads a desktop entry's icon: themed lookup of its icon string, then a
/// file load when that string is an existing path. A size of zero disables
/// icons.
pub fn load_icon_from_info(
    theme: &gtk::IconTheme,
    app: &gio::DesktopAppInfo,
    icon_size: i32,
) -> Option<Pixbuf> {
    if icon_size == 0 {
        return None;
    }
    let icon = app
        .icon()
        .and_then(|icon| gio::prelude::IconExt::to_string(&icon))?;
    if let Some(info) =
        theme.lookup_icon(icon.as_str(), icon_size, gtk::IconLookupFlags::FORCE_SIZE)
    {
        return load_icon_info(&info);
    }
    if Path::new(icon.as_str()).exists() {
        match Pixbuf::from_file_at_size(icon.as_str(), icon_size, icon_size) {
            Ok(pixbuf) => return Some(pixbuf),
            Err(error) => eprintln!("Error loading icon: {}", error.message()),
        }
    }
    None
}

/// Loads `name` as an existing file path, then as a themed icon, then falls
/// back to the theme's generic emblem. A size of zero disables icons.
pub fn load_icon_from_name(theme: &gtk::IconTheme, name: &str, icon_size: i32) -> Option<Pixbuf> {
    if icon_size == 0 {
        return None;
    }
    if !name.is_empty() {
        if Path::new(name).exists() {
            match Pixbuf::from_file_at_size(name, icon_size, icon_size) {
                Ok(pixbuf) => return Some(pixbuf),
                Err(error) => eprintln!("Error loading icon: {}", error.message()),
            }
        }
        if let Some(info) = theme.lookup_icon(name, icon_size, gtk::IconLookupFlags::FORCE_SIZE) {
            return load_icon_info(&info);
        }
    }
    let info = theme.lookup_icon(
        "emblem-generic",
        icon_size,
        gtk::IconLookupFlags::FORCE_SIZE,
    )?;
    load_icon_info(&info)
}

fn load_icon_info(info: &gtk::IconInfo) -> Option<Pixbuf> {
    match info.load_icon() {
        Ok(pixbuf) => Some(pixbuf),
        Err(error) => {
            eprintln!("Error: load_icon failed: {}", error.message());
            None
        }
    }
}
