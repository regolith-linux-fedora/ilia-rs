//! The next free workspace, for Ctrl+Enter on the Applications page: picked
//! as Regolith's "next free workspace" key picks it, named from the
//! session's `wm.workspace.NN.name` resources, and switched to before the
//! application is started there.

use std::collections::HashSet;

use crate::ipc;
use crate::keybindings::service_resources;
use crate::x11::X11Display;

/// Switches the window manager `wm_name` to the next free workspace, which
/// creates it, and returns its name once the window manager has confirmed
/// the switch.
pub fn switch_to_next_free(wm_name: &str) -> Result<String, String> {
    if !matches!(wm_name, "i3" | "sway") {
        return Err(format!("no workspaces under window manager {wm_name}"));
    }
    let names = ipc::get_workspace_names(wm_name)?;
    let number = next_free_number(names.iter().map(String::as_str));
    let name = workspace_name(number, |key| session_resource(wm_name, key));
    // The reply comes once the window manager has run the command.
    ipc::run_command(wm_name, &switch_command(wm_name, &name))?;
    Ok(name)
}

/// The number a workspace name gives its workspace: the digits it begins
/// with, as for `2` and `2:web`. A name that does not begin with a digit,
/// or begins with zero alone, gives none.
fn workspace_number(name: &str) -> Option<u64> {
    let digits = name.len() - name.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    name[..digits].parse().ok().filter(|&number| number > 0)
}

/// The smallest positive number no workspace in `names` has.
fn next_free_number<'a>(names: impl IntoIterator<Item = &'a str>) -> u64 {
    let taken: HashSet<u64> = names.into_iter().filter_map(workspace_number).collect();
    (1..).find(|number| !taken.contains(number)).unwrap_or(1)
}

/// The resource naming workspace `number`: `wm.workspace.03.name` for 3.
fn resource_key(number: u64) -> String {
    format!("wm.workspace.{number:02}.name")
}

/// The name of workspace `number`: its resource's value as `lookup` finds
/// it, unless it is missing or empty, else the number itself.
fn workspace_name(number: u64, lookup: impl FnOnce(&str) -> Option<String>) -> String {
    lookup(&resource_key(number))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| number.to_string())
}

/// The resource `key` of the session: under sway from the resource service
/// on the session bus (none when it is not running), otherwise from the X
/// display's resource database.
fn session_resource(wm_name: &str, key: &str) -> Option<String> {
    if wm_name == "sway" {
        return service_resources().remove(key);
    }
    let display = gtk::gdk::Display::default()?;
    X11Display::new(&display)?.resource(key)
}

/// The command that switches to the workspace named `name`, creating it,
/// as a numbered workspace.
fn switch_command(wm_name: &str, name: &str) -> String {
    format!("workspace number {}", quote(wm_name, name))
}

/// `text` as one argument of a window manager command, read back verbatim.
/// i3 takes `\"` and `\\` inside double quotes.
fn quote(wm_name: &str, text: &str) -> String {
    if wm_name == "sway" {
        sway_quote(text)
    } else {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// `text` as one argument of a sway command, read back verbatim.
///
/// sway unescapes nothing in commands. It drops the quote characters around
/// quoted runs, where a quote behind an odd number of backslashes neither
/// opens nor closes a run, and then expands variables: `$name` for a defined
/// `name` becomes its value and `$$` becomes `$`, while a `$` behind a
/// single backslash is left alone. So the text goes in adjacent runs, each
/// in the quote it does not hold. An odd backslash run before a quote, or at
/// the very end, stays outside the runs, where sway keeps it and that quote
/// as they are. Every other `$` is doubled.
fn sway_quote(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut quoted = String::new();
    let mut run = String::new();
    let mut at = 0;
    while at < chars.len() {
        let char = chars[at];
        if char == '\\' {
            let end = chars[at..]
                .iter()
                .position(|&char| char != '\\')
                .map_or(chars.len(), |offset| at + offset);
            let next = chars.get(end).copied();
            run.extend(&chars[at..end]);
            at = end;
            if (run.len() - run.trim_end_matches('\\').len()) % 2 == 1
                && matches!(next, None | Some('\'' | '"'))
            {
                let backslashes = run.split_off(run.trim_end_matches('\\').len());
                close_sway_run(&mut quoted, &mut run);
                quoted.push_str(&backslashes);
                quoted.extend(next);
                at += usize::from(next.is_some());
            }
            continue;
        }
        let other = match char {
            '\'' => Some('"'),
            '"' => Some('\''),
            _ => None,
        };
        if other.is_some_and(|other| run.contains(other)) {
            close_sway_run(&mut quoted, &mut run);
        }
        let escaped = at >= 1 && chars[at - 1] == '\\' && (at < 2 || chars[at - 2] != '\\');
        if char == '$' && !escaped {
            run.push('$');
        }
        run.push(char);
        at += 1;
    }
    close_sway_run(&mut quoted, &mut run);
    quoted
}

/// Appends `run` to `quoted` as a quoted run and empties it. An empty
/// `quoted` always gets a run, even an empty one, since sway only drops
/// quotes from an argument that starts with one.
fn close_sway_run(quoted: &mut String, run: &mut String) {
    if run.is_empty() && !quoted.is_empty() {
        return;
    }
    let quote = if run.contains('\'') { '"' } else { '\'' };
    quoted.push(quote);
    quoted.push_str(run);
    quoted.push(quote);
    run.clear();
}

#[cfg(test)]
mod tests {
    use super::{
        next_free_number, quote, resource_key, sway_quote, switch_command, workspace_name,
    };

    #[test]
    fn the_next_free_number_skips_every_numbered_workspace() {
        assert_eq!(next_free_number([]), 1);
        assert_eq!(next_free_number(["1", "2", "music"]), 3);
        assert_eq!(next_free_number(["1", "3"]), 2);
        assert_eq!(next_free_number(["1", "2", "2:web"]), 3);
        assert_eq!(next_free_number(["1", "2:web"]), 3);
        assert_eq!(next_free_number(["web", "music"]), 1);
        assert_eq!(next_free_number(["2", "3"]), 1);
        assert_eq!(next_free_number(["0", "1", "02"]), 3);
        assert_eq!(next_free_number(["1", " 2", "x2", "-3", "2"]), 3);
        let nine: Vec<String> = (1..10).map(|n| n.to_string()).collect();
        assert_eq!(next_free_number(nine.iter().map(String::as_str)), 10);
        assert_eq!(next_free_number(["1", "99999999999999999999999"]), 2);
    }

    #[test]
    fn workspaces_are_named_by_their_zero_padded_resource() {
        assert_eq!(resource_key(3), "wm.workspace.03.name");
        assert_eq!(resource_key(10), "wm.workspace.10.name");
        assert_eq!(resource_key(123), "wm.workspace.123.name");
        let named = |value: Option<&str>| {
            workspace_name(3, |key| {
                assert_eq!(key, "wm.workspace.03.name");
                value.map(str::to_owned)
            })
        };
        assert_eq!(named(Some("3: web mail")), "3: web mail");
        assert_eq!(named(Some("")), "3");
        assert_eq!(named(None), "3");
    }

    #[test]
    fn names_reach_the_window_manager_verbatim() {
        assert_eq!(switch_command("i3", "3"), r#"workspace number "3""#);
        assert_eq!(switch_command("sway", "3"), "workspace number '3'");
        assert_eq!(quote("i3", "3: web mail"), r#""3: web mail""#);
        assert_eq!(quote("i3", r#"3: say "hi""#), r#""3: say \"hi\"""#);
        assert_eq!(quote("i3", r"3: C:\x"), r#""3: C:\\x""#);
        assert_eq!(quote("i3", r#"3: it's "q" $x"#), r#""3: it's \"q\" $x""#);
        assert_eq!(quote("sway", "3: web; mail"), "'3: web; mail'");
        assert_eq!(quote("sway", r#"3: say "hi""#), r#"'3: say "hi"'"#);
        assert_eq!(quote("sway", "3: it's"), r#""3: it's""#);
        assert_eq!(quote("sway", r"3: C:\x"), r"'3: C:\x'");
    }

    // Each case was checked against sway 1.11 with `$mod` defined.
    #[test]
    fn sway_gets_both_quotes_backslashes_and_dollars_verbatim() {
        let cases = [
            (r#"3: it's "quoted""#, r#""3: it's "'"quoted"'"#),
            (r#"3: "'"'"#, r#"'3: "'"'"'"'"'""#),
            ("3: cost $$5", "'3: cost $$$$5'"),
            ("3: $mod-like", "'3: $$mod-like'"),
            (r"3: a\$b", r"'3: a\$b'"),
            (r"3: a\\$mod", r"'3: a\\$$mod'"),
            (r"3: $\$mod", r"'3: $$\$mod'"),
            (r"3: a\$$mod", r"'3: a\$$$mod'"),
            (r#"3: a\"b"#, r#"'3: a'\"'b'"#),
            (r"3: a\'b", r"'3: a'\''b'"),
            (r#"3: a\\"b"#, r#"'3: a\\"b'"#),
            (r#"3: q"\'"#, r#"'3: q"'\'"#),
            (r"3: tail\", r"'3: tail'\"),
            (r"3: tail\\", r"'3: tail\\'"),
            (r"3: tail\\\", r"'3: tail'\\\"),
            (r"\'x", r"''\''x'"),
            ("", "''"),
        ];
        for (name, quoted) in cases {
            assert_eq!(sway_quote(name), quoted, "{name}");
        }
    }
}
