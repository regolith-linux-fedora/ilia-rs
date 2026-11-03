use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::keybindings::navigate;
use crate::pages::DialogPage;
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
    // waits for it. Rows are sorted and the first one selected once the
    // whole PATH has been walked.
    fn load_commands(&self, model: gtk::ListStore) {
        let view = self.view.clone();
        let path = self.path.clone();
        glib::MainContext::default().spawn_local(async move {
            load_commands_from_path(&model).await;
            model.set_sort_column_id(gtk::SortColumn::Index(NAME), gtk::SortType::Ascending);
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
        "This dialog allows for the executing commands on the user path. Initially all commands are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Execute Command")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if navigate(&self.view, &self.path, event) {
            return true;
        }
        if event.keyval() == gdk::keys::constants::Return
            && self.filter.iter_first().is_none()
            && !self.entry.text().is_empty()
        {
            execute_command(self.entry.text().as_str(), self.quiet, &self.quit);
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

// Vala filter_func: lowercased query stripped of ASCII whitespace
// (g_strstrip) matched as a substring of the lowercased full path.
fn matches_query(query: &str, cmd_path: &str) -> bool {
    let query = query.to_lowercase();
    let query = query.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
    query.is_empty() || (!cmd_path.is_empty() && cmd_path.to_lowercase().contains(query))
}

fn set_selection(view: &gtk::TreeView, path: &RefCell<gtk::TreePath>) {
    let selection = view.selection();
    let (selected, _) = selection.selected_rows();
    if let Some(selected) = selected.first() {
        view.scroll_to_cell(
            Some(selected),
            None::<&gtk::TreeViewColumn>,
            false,
            0.0,
            0.0,
        );
        return;
    }
    selection.select_path(&path.borrow());
}

async fn load_commands_from_path(model: &gtk::ListStore) {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    for dir in paths.to_string_lossy().split(':') {
        let dir = gio::File::for_path(dir);
        if dir.query_exists(None::<&gio::Cancellable>) {
            if let Err(error) = load_commands_from_dir(model, &dir).await {
                eprintln!("Error: list_files failed: {}", error.message());
            }
        }
    }
}

async fn load_commands_from_dir(
    model: &gtk::ListStore,
    dir: &gio::File,
) -> Result<(), glib::Error> {
    let enumerator = dir
        .enumerate_children_future(
            gio::FILE_ATTRIBUTE_STANDARD_NAME.as_str(),
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
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
            // No executable-bit check: every enumerated name becomes a row.
            let cmd_path = dir
                .child(info.name())
                .path()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            let row = model.append();
            model.set(&row, &[(NAME, &cmd_path)]);
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

// A launch that fails surfaces as a GLib error and does not quit. The
// Vala false-without-error branch ("Error: execute_command failed",
// then quit) cannot occur here: gio-rs reports every failed launch as
// an error.
fn execute_command(cmd_path: &str, quiet: bool, quit: &Rc<dyn Fn()>) {
    let commandline = command_line(cmd_path, quiet);
    match gio::AppInfo::create_from_commandline(&commandline, None, gio::AppInfoCreateFlags::NONE)
        .and_then(|app_info| runner_app_info(&app_info))
        .and_then(|runner| runner.launch(&[], None::<&gio::AppLaunchContext>))
    {
        Ok(()) => quit(),
        Err(error) => eprintln!("Error: execute_command failed: {}", error.message()),
    }
}

#[cfg(test)]
mod tests {
    use super::{command_line, matches_query};
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
        assert!(!matches_query("x", ""));
    }
}
