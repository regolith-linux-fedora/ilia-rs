use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::filter::matches_all_words;
use crate::keybindings::navigate;
use crate::pages::{is_enter_key, select_first_row, DialogPage};
use crate::runner::runner_app_info;

const NAME: u32 = 0;
// Max number of files to read in sequence before yielding.
const FS_FILE_READ_COUNT: i32 = 64;

pub struct CommandPage {
    entry: gtk::Entry,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: Rc<RefCell<gtk::TreePath>>,
    quiet: bool,
    quit: Rc<dyn Fn()>,
}

impl CommandPage {
    pub fn new(entry: gtk::Entry, quiet: bool, quit: Rc<dyn Fn()>) -> Self {
        let model = gtk::ListStore::new(&[String::static_type()]);
        let filter = gtk::TreeModelFilter::new(&model, None);
        let entry_for_filter = entry.clone();
        filter.set_visible_func(move |tree_model, iter| {
            matches_query(
                entry_for_filter.text().as_str(),
                &string_value(tree_model, iter, NAME as i32),
            )
        });

        let view = gtk::TreeView::with_model(&filter);
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        view.selection().set_mode(gtk::SelectionMode::Single);
        let renderer = gtk::CellRendererText::new();
        let column =
            gtk::TreeViewColumn::with_attributes("Name", &renderer, &[("text", NAME as i32)]);
        column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&column);
        view.set_activate_on_single_click(true);
        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let page = Self {
            entry,
            filter,
            view,
            root: scrolled.upcast(),
            path: Rc::new(RefCell::new(gtk::TreePath::new_first())),
            quiet,
            quit,
        };
        page.connect_activation();
        page.load_commands(model);
        page
    }

    fn connect_activation(&self) {
        let quiet = self.quiet;
        let quit = self.quit.clone();
        self.view.connect_row_activated(move |view, path, _| {
            let Some(model) = view.model() else {
                return;
            };
            let Some(iter) = model.iter(path) else {
                return;
            };
            let cmd_path = string_value(&model, &iter, NAME as i32);
            if !cmd_path.is_empty() {
                execute_command(&cmd_path, quiet, &quit);
            }
        });
    }

    // Fire-and-forget scan of every PATH directory; dialog startup never
    // waits for it. The store is sorted before the scan starts, so each row
    // lands at its final position as it arrives and the end of the scan
    // never reorders the list under a highlight made while it ran. Once the
    // whole PATH has been walked, the first row is highlighted.
    fn load_commands(&self, model: gtk::ListStore) {
        let view = self.view.clone();
        let path = self.path.clone();
        model.set_sort_column_id(gtk::SortColumn::Index(NAME), gtk::SortType::Ascending);
        glib::MainContext::default().spawn_local(async move {
            load_commands_from_path(&model).await;
            set_selection(&view, &path);
        });
    }

    fn execute_selected(&self) {
        let Some(iter) = self.filter.iter(&self.path.borrow()) else {
            return;
        };
        let cmd_path = string_value(&self.filter, &iter, NAME as i32);
        if !cmd_path.is_empty() {
            execute_command(&cmd_path, self.quiet, &self.quit);
        }
    }
}

impl DialogPage for CommandPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }
    fn on_entry_changed(&self) {
        self.filter.refilter();
        set_selection(&self.view, &self.path);
    }
    fn on_entry_activated(&self) {
        self.execute_selected();
    }
    fn get_id(&self) -> &'static str {
        "terminal"
    }
    fn get_name(&self) -> &'static str {
        "<u>C</u>ommands"
    }
    fn get_icon_name(&self) -> &'static str {
        "utilities-terminal"
    }
    fn get_help(&self) -> &'static str {
        "This dialog allows for the executing commands on the user path. Initially all commands are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it. If no item matches, enter runs the typed command line, including any arguments."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Execute Command")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if navigate(&self.view, &self.path, event) {
            return true;
        }
        if is_enter_key(&event.keyval())
            && self.filter.iter_first().is_none()
            && !self.entry.text().is_empty()
        {
            // No listed command matches: run the typed command line, with
            // its arguments.
            launch(
                &typed_command_line(self.entry.text().as_str(), self.quiet),
                &self.quit,
            );
            return true;
        }
        false
    }
    fn get_keybinding(&self) -> char {
        'c'
    }
    fn show(&self) {
        self.view.grab_focus();
    }
}

fn string_value(
    model: &impl glib::IsA<gtk::TreeModel>,
    iter: &gtk::TreeIter,
    column: i32,
) -> String {
    model
        .value(iter, column)
        .get::<String>()
        .unwrap_or_default()
}

// Every word of the query must occur in the full path, ignoring case.
fn matches_query(query: &str, cmd_path: &str) -> bool {
    matches_all_words(query, &[cmd_path])
}

/// Highlights the first row: the best match for the text typed.
fn set_selection(view: &gtk::TreeView, path: &RefCell<gtk::TreePath>) {
    view.selection().set_mode(gtk::SelectionMode::Single);
    *path.borrow_mut() = select_first_row(view);
}

async fn load_commands_from_path(model: &gtk::ListStore) {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    for dir in command_dirs(&paths.to_string_lossy()) {
        let dir = gio::File::for_path(dir);
        if let Err(error) = load_commands_from_dir(model, &dir).await {
            eprintln!("Error: list_files failed: {}", error.message());
        }
    }
}

/// The directories of a PATH value that the Commands page lists, each once.
/// Entries naming the same directory are listed once, at the first: with
/// merged /usr, /bin and /sbin are links to /usr/bin and /usr/sbin, and PATH
/// may repeat an entry. Entries that are not existing directories are skipped.
fn command_dirs(path_var: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut dirs = Vec::new();
    for dir in path_var.split(':').filter(|dir| !dir.is_empty()) {
        let Ok(canonical) = std::fs::canonicalize(dir) else {
            continue;
        };
        if canonical.is_dir() && seen.insert(canonical) {
            dirs.push(dir.to_owned());
        }
    }
    dirs
}

/// Whether a PATH directory entry is a command: an executable regular file,
/// also through symbolic links. The info must be queried following links,
/// with the standard::type and access::can-execute attributes.
fn is_command(info: &gio::FileInfo) -> bool {
    info.file_type() == gio::FileType::Regular
        && info.boolean(gio::FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE.as_str())
}

async fn load_commands_from_dir(
    model: &gtk::ListStore,
    dir: &gio::File,
) -> Result<(), glib::Error> {
    // Follow links: most commands are links (alternatives), and is_command()
    // needs their targets.
    let attributes = format!(
        "{},{},{}",
        gio::FILE_ATTRIBUTE_STANDARD_NAME.as_str(),
        gio::FILE_ATTRIBUTE_STANDARD_TYPE.as_str(),
        gio::FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE.as_str()
    );
    let enumerator = dir
        .enumerate_children_future(
            &attributes,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
        )
        .await?;
    loop {
        let files = enumerator
            .next_files_future(FS_FILE_READ_COUNT, glib::Priority::DEFAULT)
            .await?;
        if files.is_empty() {
            return Ok(());
        }
        for info in files {
            if !is_command(&info) {
                continue;
            }
            let cmd_path = dir
                .child(info.name())
                .path()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Inserted at its sorted position, with one row-inserted signal.
            model.insert_with_values(None, &[(NAME, &cmd_path)]);
        }
    }
}

// Vala execute_command: -q runs the path itself; otherwise the path is
// single-quoted into an x-terminal-emulator command line.
fn command_line(cmd_path: &str, quiet: bool) -> String {
    if quiet {
        cmd_path.to_owned()
    } else {
        format!("x-terminal-emulator -e '{cmd_path}'")
    }
}

// A command line typed by the user, in shell syntax: -q runs it as is;
// otherwise x-terminal-emulator runs the program and its arguments given
// after -e. AppInfo expands % field codes as in a desktop file's Exec key,
// so literal % signs are doubled.
fn typed_command_line(text: &str, quiet: bool) -> String {
    let text = text.replace('%', "%%");
    if quiet {
        text
    } else {
        format!("x-terminal-emulator -e {text}")
    }
}

fn execute_command(cmd_path: &str, quiet: bool, quit: &Rc<dyn Fn()>) {
    launch(&command_line(cmd_path, quiet), quit);
}

// A launch that fails surfaces as a GLib error and does not quit. The
// Vala false-without-error branch ("Error: execute_command failed",
// then quit) cannot occur here: gio-rs reports every failed launch as
// an error.
fn launch(commandline: &str, quit: &Rc<dyn Fn()>) {
    match gio::AppInfo::create_from_commandline(commandline, None, gio::AppInfoCreateFlags::NONE)
        .and_then(|app_info| runner_app_info(&app_info))
        .and_then(|runner| runner.launch(&[], None::<&gio::AppLaunchContext>))
    {
        Ok(()) => quit(),
        Err(error) => eprintln!("Error: execute_command failed: {}", error.message()),
    }
}

#[cfg(test)]
mod tests {
    use super::{command_dirs, command_line, is_command, matches_query, typed_command_line};
    use crate::runner::runner_app_info;
    use gio::prelude::*;
    use std::os::unix::fs::PermissionsExt;

    // Parse the launched command line the way GIO does and drop the field
    // codes that expand to nothing when launching without files.
    fn wrapped_argv(commandline: &str) -> Vec<String> {
        let app_info =
            gio::AppInfo::create_from_commandline(commandline, None, gio::AppInfoCreateFlags::NONE)
                .unwrap();
        let runner = runner_app_info(&app_info).unwrap();
        let line = runner.commandline().unwrap();
        glib::shell_parse_argv(line)
            .unwrap()
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .filter(|arg| arg != "%f")
            .collect()
    }

    #[test]
    fn command_lines_match_vala_shapes() {
        assert_eq!(command_line("/bin/tool", true), "/bin/tool");
        assert_eq!(
            command_line("/bin/tool", false),
            "x-terminal-emulator -e '/bin/tool'"
        );
    }

    #[test]
    fn typed_command_lines_keep_their_arguments() {
        assert_eq!(
            typed_command_line("tool --flag 'two words'", true),
            "tool --flag 'two words'"
        );
        assert_eq!(
            typed_command_line("tool --flag 'two words'", false),
            "x-terminal-emulator -e tool --flag 'two words'"
        );
        assert_eq!(typed_command_line("printf 100%", true), "printf 100%%");
    }

    #[test]
    fn argv_shapes_go_through_the_systemd_wrapper() {
        let dir = std::env::temp_dir().join(format!("ilia-terminal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("systemd-run");
        std::fs::write(&runner, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", &dir);

        let terminal = wrapped_argv(&command_line("/fixture bin/tool", false));
        let quiet = wrapped_argv(&command_line("/fixture/tool", true));
        let typed = wrapped_argv(&typed_command_line("/fixture/tool 'two words'", false));

        match saved {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            terminal[..4],
            ["systemd-run", "--user", "--scope", "--unit"]
        );
        assert!(terminal[4].starts_with("run_ilia_x\\x2dterminal\\x2demulator"));
        assert_eq!(
            terminal[5..],
            ["x-terminal-emulator", "-e", "/fixture bin/tool"]
        );
        assert_eq!(quiet[..4], ["systemd-run", "--user", "--scope", "--unit"]);
        assert!(quiet[4].starts_with("run_ilia_-fixture-tool"));
        assert_eq!(quiet[5..], ["/fixture/tool"]);
        assert_eq!(
            typed[5..],
            ["x-terminal-emulator", "-e", "/fixture/tool", "two words"]
        );
    }

    #[test]
    fn filter_is_case_insensitive_stripped_substring() {
        assert!(matches_query("", "/bin/anything"));
        assert!(matches_query("  \t", "/bin/anything"));
        assert!(matches_query(
            " Fixture-COMMAND\n",
            "/tmp/bin/fixture-command"
        ));
        assert!(matches_query("BIN/FIX", "/tmp/bin/Fixture-command"));
        assert!(!matches_query(
            "free-command hello",
            "/tmp/bin/free-command"
        ));
        assert!(!matches_query("fixture", "/tmp/bin/free-command"));
        assert!(matches_query("COMMAND  free", "/tmp/bin/free-command"));
        assert!(!matches_query("x", ""));
    }

    #[test]
    fn command_dirs_list_each_directory_once() {
        let root = std::env::temp_dir().join(format!("ilia-command-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let usr_bin = root.join("usr/bin");
        std::fs::create_dir_all(&usr_bin).unwrap();
        let bin = root.join("bin");
        std::os::unix::fs::symlink(&usr_bin, &bin).unwrap();
        let file = root.join("file");
        std::fs::write(&file, "").unwrap();
        let missing = root.join("missing");
        let (usr_bin, bin, file, missing) = (
            usr_bin.to_str().unwrap().to_owned(),
            bin.to_str().unwrap().to_owned(),
            file.to_str().unwrap().to_owned(),
            missing.to_str().unwrap().to_owned(),
        );

        let path_var = format!("{bin}::{usr_bin}:{missing}:{file}:{bin}");
        let dirs = command_dirs(&path_var);
        let _ = std::fs::remove_dir_all(&root);

        // /bin and /usr/bin are one directory, listed under the first name.
        assert_eq!(dirs, [bin]);
        assert!(command_dirs("").is_empty());
    }

    #[test]
    fn only_executable_files_are_commands() {
        let info = |file_type, executable| {
            let info = gio::FileInfo::new();
            info.set_file_type(file_type);
            info.set_attribute_boolean(gio::FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE.as_str(), executable);
            info
        };
        assert!(is_command(&info(gio::FileType::Regular, true)));
        assert!(!is_command(&info(gio::FileType::Regular, false)));
        assert!(!is_command(&info(gio::FileType::Directory, true)));
    }
}
