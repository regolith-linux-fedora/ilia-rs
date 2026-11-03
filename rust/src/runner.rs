use gio::prelude::*;

pub fn runner_app_info(app_info: &gio::AppInfo) -> Result<gio::AppInfo, glib::Error> {
    if glib::find_program_in_path("systemd-run").is_none() {
        return Ok(app_info.clone());
    }

    let app_id = app_info
        .id()
        .map(|value| value.to_string())
        .unwrap_or_else(|| {
            app_info
                .commandline()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let commandline = app_info
        .commandline()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let random = glib::uuid_string_random();
    let unit = format!(
        "run_ilia_{}_{}.scope",
        systemd_escape(&app_id),
        &random[..8]
    );
    let wrapped = format!("systemd-run --user --scope --unit \"{unit}\" {commandline}");

    gio::AppInfo::create_from_commandline(&wrapped, Some(&app_id), gio::AppInfoCreateFlags::NONE)
}

pub fn systemd_escape(value: &str) -> String {
    let mut escaped = String::new();
    for (index, byte) in value.bytes().enumerate() {
        if index == 0 {
            if byte == b'.' {
                escaped.push_str("\\x2e");
            } else {
                escaped.push(char::from(byte));
            }
        } else if (byte > 31 && byte < 46)
            || (byte > 58 && byte < 65)
            || (byte > 90 && byte < 97 && byte != b'_')
            || byte > 122
        {
            escaped.push_str(&format!("\\x{byte:x}"));
        } else {
            escaped.push(char::from(byte));
        }
    }
    escaped.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::systemd_escape;

    #[test]
    fn escape_matches_the_vala_unit_name_rules() {
        assert_eq!(systemd_escape("alpha.desktop"), "alpha.desktop");
        assert_eq!(systemd_escape("/usr/bin/tool"), "-usr-bin-tool");
        assert_eq!(systemd_escape(".hidden name"), "\\x2ehidden\\x20name");
        assert_eq!(systemd_escape("-hyphen"), "-hyphen");
        assert_eq!(systemd_escape("a-hyphen"), "a\\x2dhyphen");
        assert_eq!(systemd_escape("a\u{1f}control"), "a\u{1f}control");
    }
}
