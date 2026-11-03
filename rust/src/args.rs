use std::collections::HashMap;

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Args {
    pub cmd: Option<String>,
    pub arg: HashMap<String, Option<String>>,
}

impl Args {
    pub fn parse<I, S>(tokens: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut tokens = tokens.into_iter().map(Into::into);
        let cmd = tokens.next();
        let mut arg = HashMap::new();
        let mut pending: Option<String> = None;

        for token in tokens {
            if token.starts_with('-') {
                if let Some(key) = pending.replace(token) {
                    arg.insert(key, None);
                }
            } else if let Some(key) = pending.take() {
                arg.insert(key, Some(token));
            }
        }

        if let Some(key) = pending {
            arg.insert(key, None);
        }

        Self { cmd, arg }
    }

    pub fn has(&self, key: &str) -> bool {
        self.arg.contains_key(key)
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.arg.get(key).and_then(|value| value.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use std::collections::HashMap;

    fn expected(
        cmd: Option<&str>,
        arg: impl IntoIterator<Item = (&'static str, Option<&'static str>)>,
    ) -> Args {
        Args {
            cmd: cmd.map(str::to_owned),
            arg: arg
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.map(str::to_owned)))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn empty_input_has_no_command_or_flags() {
        assert_eq!(Args::parse(Vec::<String>::new()), Args::default());
    }

    #[test]
    fn first_token_is_stored_as_command() {
        assert_eq!(
            Args::parse(["/usr/bin/ilia"]),
            expected(Some("/usr/bin/ilia"), [])
        );
    }

    #[test]
    fn flag_followed_by_bare_token_stores_its_value() {
        assert_eq!(
            Args::parse(["ilia", "-p", "apps"]),
            expected(Some("ilia"), [("-p", Some("apps"))]),
        );
    }

    #[test]
    fn trailing_flag_is_stored_with_null_value() {
        assert_eq!(
            Args::parse(["ilia", "-n"]),
            expected(Some("ilia"), [("-n", None)]),
        );
    }

    #[test]
    fn new_flag_flushes_pending_flag_with_null_value() {
        assert_eq!(
            Args::parse(["ilia", "-n", "-p", "apps"]),
            expected(Some("ilia"), [("-n", None), ("-p", Some("apps"))],),
        );
    }

    #[test]
    fn bare_tokens_without_pending_flag_are_ignored() {
        assert_eq!(
            Args::parse(["ilia", "ignored", "-p", "apps", "also-ignored"]),
            expected(Some("ilia"), [("-p", Some("apps"))]),
        );
    }

    #[test]
    fn documented_switch_forms_are_supported() {
        assert_eq!(
            Args::parse([
                "ilia",
                "-n",
                "-a",
                "-q",
                "-p",
                "apps",
                "-t",
                "file.css",
                "-l",
                "Label",
                "-i",
                "icon-name",
                "-h",
                "--help",
                "-v",
                "--version",
            ]),
            expected(
                Some("ilia"),
                [
                    ("-n", None),
                    ("-a", None),
                    ("-q", None),
                    ("-p", Some("apps")),
                    ("-t", Some("file.css")),
                    ("-l", Some("Label")),
                    ("-i", Some("icon-name")),
                    ("-h", None),
                    ("--help", None),
                    ("-v", None),
                    ("--version", None),
                ],
            ),
        );
    }
}
