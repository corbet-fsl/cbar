#[cfg(feature = "workspaces+sway")]
use super::{Visibility, Workspace};
#[cfg(any(
    feature = "workspaces+sway",
    feature = "keyboard+sway",
    feature = "bindmode+sway"
))]
use crate::await_sync;
#[cfg(any(
    feature = "workspaces+sway",
    feature = "keyboard+sway",
    feature = "bindmode+sway"
))]
use crate::clients::sway::Client;
#[cfg(feature = "bindmode+sway")]
use crate::clients::sway::ModeMessage;
#[cfg(feature = "keyboard+sway")]
use crate::clients::sway::{InputChange, InputEvent, InputMessage};
#[cfg(feature = "workspaces+sway")]
use crate::clients::sway::{
    Node, Workspace as SwayWorkspace, WorkspaceChange, WorkspaceEvent, WorkspaceMessage,
};
#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
use crate::{error, spawn};
#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
use color_eyre::Report;
#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
use std::ffi::OsString;
#[cfg(feature = "workspaces+sway")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(
    feature = "workspaces+sway",
    feature = "keyboard+sway",
    feature = "bindmode+sway"
))]
use tokio::sync::broadcast::{Receiver, channel};

#[cfg(feature = "workspaces+sway")]
use super::{WorkspaceTarget, WorkspaceUpdate};

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandDialect {
    Sway,
    I3,
}

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn active_command_dialect() -> CommandDialect {
    command_dialect_from_values(
        std::env::var_os("SCROLLSOCK"),
        std::env::var_os("SWAYSOCK"),
        std::env::var_os("I3SOCK"),
    )
}

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn command_dialect_from_values(
    scrollsock: Option<OsString>,
    swaysock: Option<OsString>,
    i3sock: Option<OsString>,
) -> CommandDialect {
    if scrollsock.is_some_and(|path| !path.is_empty())
        || swaysock.is_some_and(|path| !path.is_empty())
    {
        CommandDialect::Sway
    } else if i3sock.is_some_and(|path| !path.is_empty()) {
        CommandDialect::I3
    } else {
        // Client creation without either variable is allowed so supervisors
        // can wait for a socket. Sway is the conservative project default.
        CommandDialect::Sway
    }
}

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn quote_command_argument(value: &str, dialect: CommandDialect) -> Result<String, Report> {
    match dialect {
        CommandDialect::Sway => quote_sway_argument(value),
        CommandDialect::I3 => quote_i3_argument(value),
    }
}

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn quote_sway_argument(value: &str) -> Result<String, Report> {
    if value
        .chars()
        .any(|character| matches!(character, '\0' | '\n' | '\r'))
    {
        return Err(Report::msg(
            "Sway command arguments cannot contain NUL or line breaks",
        ));
    }

    // Sway's runtime parser retains backslashes, but a quote preceded by an
    // odd run of them is escaped. Choose a delimiter for which every interior
    // occurrence is already escaped by the value itself. A trailing odd run
    // would escape our closing delimiter and is therefore unrepresentable.
    let trailing_backslashes = value.chars().rev().take_while(|&ch| ch == '\\').count();
    if trailing_backslashes % 2 != 0 {
        return Err(Report::msg(
            "Sway command arguments ending in an odd number of backslashes cannot be represented exactly",
        ));
    }

    let delimiter = ['"', '\'']
        .into_iter()
        .find(|&candidate| quotes_are_parser_safe(value, candidate))
        .ok_or_else(|| {
            Report::msg(
                "Sway command argument quotes cannot be represented exactly by the runtime parser",
            )
        })?;

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push(delimiter);
    let mut preceding_backslashes = 0;
    for character in value.chars() {
        if character == '$' {
            // Runtime commands perform variable expansion after quote removal.
            // Sway's documented `$$` escape yields one literal dollar sign.
            // One immediately preceding backslash is Sway's other literal-$
            // escape and is already part of the requested value.
            if preceding_backslashes != 1 {
                quoted.push('$');
            }
        }
        quoted.push(character);
        if character == '\\' {
            preceding_backslashes += 1;
        } else {
            preceding_backslashes = 0;
        }
    }
    quoted.push(delimiter);
    Ok(quoted)
}

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn quotes_are_parser_safe(value: &str, delimiter: char) -> bool {
    let mut preceding_backslashes = 0;
    for character in value.chars() {
        if character == delimiter && preceding_backslashes % 2 == 0 {
            return false;
        }
        if character == '\\' {
            preceding_backslashes += 1;
        } else {
            preceding_backslashes = 0;
        }
    }
    true
}

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn quote_i3_argument(value: &str) -> Result<String, Report> {
    if value
        .chars()
        .any(|character| matches!(character, '\0' | '\n' | '\r'))
    {
        return Err(Report::msg(
            "i3 command arguments cannot contain NUL or line breaks",
        ));
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    Ok(quoted)
}

#[cfg(feature = "keyboard+sway")]
fn next_keyboard_layout_command(
    identifier: &str,
    dialect: CommandDialect,
) -> Result<String, Report> {
    Ok(format!(
        "input {} xkb_switch_layout next",
        quote_command_argument(identifier, dialect)?
    ))
}

#[cfg(feature = "keyboard+sway")]
fn keyboard_layout_from_inputs(
    inputs: &[crate::clients::sway::Input],
) -> Option<KeyboardLayoutUpdate> {
    inputs
        .iter()
        .find_map(|input| input.xkb_active_layout_name.clone())
        .map(KeyboardLayoutUpdate)
}

#[cfg(feature = "workspaces+sway")]
fn focus_command_for_target(
    target: &WorkspaceTarget,
    workspace: Option<&SwayWorkspace>,
    dialect: CommandDialect,
) -> Result<String, Report> {
    match target.persistent_by_name() {
        WorkspaceTarget::Name(name) => Ok(format!(
            "workspace {}",
            quote_command_argument(&name, dialect)?
        )),
        WorkspaceTarget::Id(id) => {
            let workspace = workspace
                .ok_or_else(|| Report::msg(format!("couldn't find workspace with id {id}")))?;

            // Numbered workspaces are focused by number rather than name.
            // The name can be changed by another process (e.g. a
            // workspace renamer reacting to the focus-change this same
            // command causes) in the gap between the query above and the
            // command below, which previously made sway create a new,
            // empty workspace instead of focusing the existing one. The
            // number is stable across such renames, so prefer it when
            // available; sway reports -1 for workspaces with no number.
            if workspace.num >= 0 {
                Ok(format!("workspace number {}", workspace.num))
            } else {
                Ok(format!(
                    "workspace {}",
                    quote_command_argument(&workspace.name, dialect)?
                ))
            }
        }
        WorkspaceTarget::Persistent { .. } => unreachable!("persistent target was resolved"),
    }
}

#[cfg(feature = "workspaces+sway")]
fn workspace_snapshot_update(
    initialized: &AtomicBool,
    workspaces: &[SwayWorkspace],
) -> WorkspaceUpdate {
    let workspaces = workspaces.iter().cloned().map(Workspace::from).collect();
    if initialized.swap(true, Ordering::AcqRel) {
        WorkspaceUpdate::Resync(workspaces)
    } else {
        WorkspaceUpdate::Init(workspaces)
    }
}

#[cfg(feature = "workspaces+sway")]
impl super::WorkspaceClient for Client {
    fn focus(&self, target: WorkspaceTarget) {
        let client = self.connection().clone();
        spawn(async move {
            let mut client = client.lock().await;

            let workspace = if let WorkspaceTarget::Id(id) = target.persistent_by_name() {
                client
                    .get_workspaces()
                    .await?
                    .into_iter()
                    .find(|workspace| workspace.id == id)
            } else {
                None
            };

            let command =
                focus_command_for_target(&target, workspace.as_ref(), active_command_dialect())?;

            if let Err(e) = client.run_command(command).await {
                return Err(Report::msg(format!(
                    "Couldn't focus workspace '{target:?}': {e:#}"
                )));
            }

            Ok(())
        });
    }

    fn subscribe(&self) -> Receiver<WorkspaceUpdate> {
        let (tx, rx) = channel(16);
        let alive_tx = tx.clone();
        let initialized = AtomicBool::new(false);

        await_sync(async {
            if let Err(error) = self
                .add_workspace_listener(
                    move || alive_tx.receiver_count() > 0,
                    move |message| {
                        let update = match message {
                            WorkspaceMessage::Snapshot(workspaces) => {
                                workspace_snapshot_update(&initialized, workspaces)
                            }
                            WorkspaceMessage::Event(event) => WorkspaceUpdate::from(event.clone()),
                        };
                        tx.send(update).is_ok()
                    },
                )
                .await
            {
                error!("Failed to register Sway workspace listener: {error:#}");
            }
        });

        rx
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<Node> for Workspace {
    fn from(node: Node) -> Self {
        let visibility = Visibility::from(&node);

        Self {
            id: node.id,
            index: node.num.unwrap_or(0),
            name: node.name.unwrap_or_default(),
            monitor: node.output.unwrap_or_default(),
            visibility,
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<SwayWorkspace> for Workspace {
    fn from(workspace: SwayWorkspace) -> Self {
        let visibility = Visibility::from(&workspace);

        Self {
            id: workspace.id,
            index: workspace.num,
            name: workspace.name,
            monitor: workspace.output,
            visibility,
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<&Node> for Visibility {
    fn from(node: &Node) -> Self {
        if node.focused {
            Self::focused()
        } else if node.visible.unwrap_or(false) {
            Self::visible()
        } else {
            Self::Hidden
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<&SwayWorkspace> for Visibility {
    fn from(workspace: &SwayWorkspace) -> Self {
        if workspace.focused {
            Self::focused()
        } else if workspace.visible {
            Self::visible()
        } else {
            Self::Hidden
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<WorkspaceEvent> for WorkspaceUpdate {
    fn from(event: WorkspaceEvent) -> Self {
        match event.change {
            WorkspaceChange::Init => event
                .current
                .map(Workspace::from)
                .map(Self::Add)
                .unwrap_or(Self::Unknown),
            WorkspaceChange::Empty => event
                .current
                .or(event.old)
                .map(|workspace| Self::Remove(workspace.id))
                .unwrap_or(Self::Unknown),
            WorkspaceChange::Focus => event.current.map_or(Self::Unknown, |current| Self::Focus {
                old: event.old.map(Workspace::from),
                new: Workspace::from(current),
            }),
            WorkspaceChange::Move => event
                .current
                .map(Workspace::from)
                .map(Self::Move)
                .unwrap_or(Self::Unknown),
            WorkspaceChange::Rename => {
                if let Some(node) = event.current {
                    Self::Rename {
                        id: node.id,
                        name: node.name.unwrap_or_default(),
                    }
                } else {
                    Self::Unknown
                }
            }
            WorkspaceChange::Urgent => {
                if let Some(node) = event.current {
                    Self::Urgent {
                        id: node.id,
                        urgent: node.urgent,
                    }
                } else {
                    Self::Unknown
                }
            }
            _ => Self::Unknown,
        }
    }
}

#[cfg(all(test, feature = "workspaces+sway"))]
mod tests {
    use super::*;

    #[test]
    fn closed_numbered_favourite_focuses_by_name() {
        let target = WorkspaceTarget::Persistent {
            name: "2".to_string(),
            index: Some(2),
        };

        assert_eq!(
            focus_command_for_target(&target, None, CommandDialect::Sway)
                .expect("named target to produce a command"),
            "workspace \"2\""
        );
    }

    #[test]
    fn open_numbered_workspace_keeps_stable_number_command() {
        let target = WorkspaceTarget::Id(91);
        let workspace = SwayWorkspace {
            id: 91,
            num: 8,
            name: "renamed meanwhile".to_string(),
            output: "DP-1".to_string(),
            visible: false,
            focused: false,
        };
        assert_eq!(
            focus_command_for_target(&target, Some(&workspace), CommandDialect::Sway)
                .expect("numbered workspace to produce a command"),
            "workspace number 8"
        );
    }

    #[test]
    fn first_workspace_snapshot_initializes_and_later_snapshot_resyncs() {
        let initialized = AtomicBool::new(false);
        let initial = [SwayWorkspace {
            id: 1,
            num: 1,
            name: "old".to_string(),
            output: "DP-1".to_string(),
            visible: true,
            focused: true,
        }];
        assert!(matches!(
            workspace_snapshot_update(&initialized, &initial),
            WorkspaceUpdate::Init(workspaces)
                if workspaces.len() == 1 && workspaces[0].name == "old"
        ));

        let resnapshot = [SwayWorkspace {
            id: 1,
            num: 1,
            name: "renamed".to_string(),
            output: "DP-1".to_string(),
            visible: true,
            focused: true,
        }];
        assert!(matches!(
            workspace_snapshot_update(&initialized, &resnapshot),
            WorkspaceUpdate::Resync(workspaces)
                if workspaces.len() == 1 && workspaces[0].name == "renamed"
        ));
    }

    #[test]
    fn sway_runtime_parser_round_trips_every_supported_argument() {
        for value in [
            "",
            "plain",
            "spaces and\ttabs",
            "x; exec danger",
            "comma, exec danger",
            "single'quote",
            "double\"quote",
            "escaped\\\"double'quote",
            "middle\\backslash",
            "even-trailing\\\\",
            "$danger",
            "$$danger",
            "escaped\\$danger",
            "double-slash\\\\$danger",
        ] {
            let command = focus_command_for_target(
                &WorkspaceTarget::Name(value.to_string()),
                None,
                CommandDialect::Sway,
            )
            .expect("supported value should produce a command");
            let commands = parse_sway_runtime_command(&command);
            assert_eq!(
                commands,
                vec![vec!["workspace".to_string(), value.to_string()]]
            );
        }

        for value in [
            "bad\nexec danger",
            "bad\rtail",
            "bad\0tail",
            "odd-trailing\\",
            "both\"quote'kinds",
        ] {
            assert!(
                quote_sway_argument(value).is_err(),
                "unrepresentable value {value:?} should be rejected"
            );
        }

        #[cfg(feature = "keyboard+sway")]
        {
            let value = "kbd; exec danger";
            let command = next_keyboard_layout_command(value, CommandDialect::Sway)
                .expect("injection-shaped identifier should remain one argument");
            assert_eq!(
                parse_sway_runtime_command(&command),
                vec![vec![
                    "input".to_string(),
                    value.to_string(),
                    "xkb_switch_layout".to_string(),
                    "next".to_string(),
                ]]
            );
        }
    }

    #[test]
    fn every_accepted_short_argument_round_trips_through_sway_runtime_parser() {
        let alphabet = ['a', '"', '\'', '\\', '$', ';', ',', ' ', '\t', '[', ']'];
        for length in 0_usize..=4 {
            for mut encoded_value in 0..alphabet.len().pow(length as u32) {
                let mut value = String::with_capacity(length);
                for _ in 0..length {
                    value.push(alphabet[encoded_value % alphabet.len()]);
                    encoded_value /= alphabet.len();
                }

                let target = WorkspaceTarget::Name(value.clone());
                let Ok(command) = focus_command_for_target(&target, None, CommandDialect::Sway)
                else {
                    continue;
                };
                assert_eq!(
                    parse_sway_runtime_command(&command),
                    vec![vec!["workspace".to_string(), value.clone()]],
                    "accepted value {value:?} must remain one exact argument"
                );
            }
        }
    }

    #[test]
    fn i3_socket_selects_i3_parser_and_round_trips_its_escape_rules() {
        assert_eq!(
            command_dialect_from_values(None, None, Some(OsString::from("/run/i3.sock"))),
            CommandDialect::I3
        );
        assert_eq!(
            command_dialect_from_values(
                Some(OsString::from("/run/scroll.sock")),
                Some(OsString::from("/run/sway.sock")),
                Some(OsString::from("/run/i3.sock")),
            ),
            CommandDialect::Sway
        );

        for value in [
            "",
            "plain",
            "spaces and\ttabs",
            "x; exec danger",
            "comma, exec danger",
            "single'quote",
            "double\"quote",
            "middle\\backslash",
            "trailing\\",
            "$danger",
            "[brackets]",
        ] {
            let command = focus_command_for_target(
                &WorkspaceTarget::Name(value.to_string()),
                None,
                CommandDialect::I3,
            )
            .expect("i3 value should produce a command");
            assert_eq!(
                parse_i3_workspace_command(&command).as_deref(),
                Some(value),
                "i3 runtime parser must recover {value:?} exactly"
            );
        }

        assert!(quote_i3_argument("bad\nexec danger").is_err());
        assert!(quote_i3_argument("bad\0tail").is_err());
    }

    #[test]
    fn every_short_i3_argument_round_trips_through_i3_runtime_parser() {
        let alphabet = ['a', '"', '\'', '\\', '$', ';', ',', ' ', '\t', '[', ']'];
        for length in 0_usize..=4 {
            for mut encoded_value in 0..alphabet.len().pow(length as u32) {
                let mut value = String::with_capacity(length);
                for _ in 0..length {
                    value.push(alphabet[encoded_value % alphabet.len()]);
                    encoded_value /= alphabet.len();
                }
                let command = focus_command_for_target(
                    &WorkspaceTarget::Name(value.clone()),
                    None,
                    CommandDialect::I3,
                )
                .expect("short i3 value should be representable");
                assert_eq!(
                    parse_i3_workspace_command(&command),
                    Some(value.clone()),
                    "i3 accepted value {value:?} must remain one exact argument"
                );
            }
        }
    }

    /// Mirrors i3's runtime `commands_parser.c::parse_string`: double quotes
    /// delimit a word, and only escaped double quotes/backslashes are
    /// unescaped while copying it into the command stack.
    fn parse_i3_workspace_command(command: &str) -> Option<String> {
        let value = command.strip_prefix("workspace ")?;
        let bytes = value.as_bytes();
        if bytes.first() != Some(&b'"') {
            return None;
        }

        let mut parsed = Vec::with_capacity(value.len());
        let mut index = 1;
        let mut closed = false;
        while index < bytes.len() {
            match bytes[index] {
                b'"' => {
                    index += 1;
                    closed = true;
                    break;
                }
                b'\\' if index + 1 < bytes.len() => {
                    let next = bytes[index + 1];
                    if matches!(next, b'"' | b'\\') {
                        parsed.push(next);
                    } else {
                        parsed.extend_from_slice(&[b'\\', next]);
                    }
                    index += 2;
                }
                byte => {
                    parsed.push(byte);
                    index += 1;
                }
            }
        }

        if !closed || !value[index..].trim().is_empty() {
            return None;
        }
        String::from_utf8(parsed).ok()
    }

    /// Mirrors Sway's runtime `argsep` -> `split_args` -> `strip_quotes` ->
    /// `do_var_replacement` path. This intentionally does not model the
    /// config-file parser, which additionally unescapes arguments.
    fn parse_sway_runtime_command(command: &str) -> Vec<Vec<String>> {
        split_sway(command, |character| matches!(character, ';' | ','), false)
            .into_iter()
            .filter(|command| !command.trim().is_empty())
            .map(|command| {
                let mut arguments = split_sway(&command, char::is_whitespace, true);
                for argument in arguments.iter_mut().skip(1) {
                    if matches!(argument.chars().next(), Some('"' | '\'')) {
                        *argument = strip_sway_quotes(argument);
                    }
                    *argument = replace_sway_variables(argument);
                }
                arguments
            })
            .collect()
    }

    fn split_sway(
        input: &str,
        delimiter: impl Fn(char) -> bool,
        brackets_group: bool,
    ) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut token = String::new();
        let mut in_string = false;
        let mut in_char = false;
        let mut in_brackets = false;
        let mut escaped = false;

        for character in input.chars() {
            let is_delimiter = delimiter(character);
            if character == '"' && !in_char && !escaped {
                in_string = !in_string;
            } else if character == '\'' && !in_string && !escaped {
                in_char = !in_char;
            } else if brackets_group
                && character == '['
                && !in_string
                && !in_char
                && !in_brackets
                && !escaped
            {
                in_brackets = true;
            } else if brackets_group
                && character == ']'
                && !in_string
                && !in_char
                && in_brackets
                && !escaped
            {
                in_brackets = false;
            } else if is_delimiter && !in_string && !in_char && !in_brackets && !escaped {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
                escaped = false;
                continue;
            }

            token.push(character);
            if character == '\\' {
                escaped = !escaped;
            } else {
                escaped = false;
            }
        }
        if !token.is_empty() {
            tokens.push(token);
        }
        tokens
    }

    fn strip_sway_quotes(value: &str) -> String {
        let mut stripped = String::with_capacity(value.len());
        let mut in_string = false;
        let mut in_char = false;
        let mut escaped = false;
        for character in value.chars() {
            if character == '\'' && !in_string && !escaped {
                in_char = !in_char;
                continue;
            }
            if character == '"' && !in_char && !escaped {
                in_string = !in_string;
                continue;
            }
            stripped.push(character);
            if character == '\\' {
                escaped = !escaped;
            } else {
                escaped = false;
            }
        }
        stripped
    }

    fn replace_sway_variables(value: &str) -> String {
        let mut value = value.to_string();
        let mut search_from = 0;
        while let Some(relative) = value[search_from..].find('$') {
            let dollar = search_from + relative;
            let bytes = value.as_bytes();
            let escaped = dollar > 0
                && bytes[dollar - 1] == b'\\'
                && (dollar == 1 || bytes[dollar - 2] != b'\\');
            if escaped {
                search_from = dollar + 1;
            } else if bytes.get(dollar + 1) == Some(&b'$') {
                value.remove(dollar);
                search_from = dollar + 1;
            } else if value[dollar..].starts_with("$danger") {
                value.replace_range(dollar..dollar + "$danger".len(), "exec danger");
                search_from = dollar + "exec danger".len();
            } else {
                search_from = dollar + 1;
            }
        }
        value
    }

    #[test]
    fn incomplete_workspace_events_degrade_without_panicking() {
        let missing = WorkspaceUpdate::from(WorkspaceEvent {
            change: WorkspaceChange::Focus,
            current: None,
            old: None,
        });
        assert!(matches!(missing, WorkspaceUpdate::Unknown));

        let removed = WorkspaceUpdate::from(WorkspaceEvent {
            change: WorkspaceChange::Empty,
            current: None,
            old: Some(Node {
                id: 44,
                num: Some(4),
                name: Some("4".to_string()),
                output: Some("DP-1".to_string()),
                visible: Some(false),
                focused: false,
                urgent: false,
            }),
        });
        assert!(matches!(removed, WorkspaceUpdate::Remove(44)));
    }
}

#[cfg(feature = "keyboard+sway")]
use super::{KeyboardLayoutClient, KeyboardLayoutUpdate};

#[cfg(feature = "keyboard+sway")]
impl KeyboardLayoutClient for Client {
    fn set_next_active(&self) {
        let client = self.connection().clone();
        spawn(async move {
            let mut client = client.lock().await;

            let inputs = match client.get_inputs().await {
                Ok(inputs) => inputs,
                Err(error) => {
                    error!("Failed to get Sway inputs: {error:#}");
                    return;
                }
            };

            if let Some(keyboard) = inputs
                .into_iter()
                .find(|i| i.xkb_active_layout_name.is_some())
            {
                let command = match next_keyboard_layout_command(
                    &keyboard.identifier,
                    active_command_dialect(),
                ) {
                    Ok(command) => command,
                    Err(error) => {
                        error!("Failed to build Sway keyboard command: {error:#}");
                        return;
                    }
                };
                if let Err(e) = client.run_command(command).await {
                    error!("Failed to switch keyboard layout due to Sway error: {e}");
                }
            } else {
                error!("Failed to get keyboard identifier from Sway");
            }
        });
    }

    fn subscribe(&self) -> Receiver<KeyboardLayoutUpdate> {
        let (tx, rx) = channel(16);
        let alive_tx = tx.clone();

        await_sync(async {
            if let Err(error) = self
                .add_input_listener(
                    move || alive_tx.receiver_count() > 0,
                    move |message| match message {
                        InputMessage::Snapshot(inputs) => {
                            if let Some(layout) = keyboard_layout_from_inputs(inputs) {
                                tx.send(layout).is_ok()
                            } else {
                                error!("Failed to get keyboard layout from Sway!");
                                true
                            }
                        }
                        InputMessage::Event(event) => {
                            if let Ok(layout) = KeyboardLayoutUpdate::try_from(event.clone()) {
                                tx.send(layout).is_ok()
                            } else {
                                true
                            }
                        }
                    },
                )
                .await
            {
                error!("Failed to register Sway input listener: {error:#}");
            }
        });

        rx
    }
}

#[cfg(feature = "keyboard+sway")]
impl TryFrom<InputEvent> for KeyboardLayoutUpdate {
    type Error = ();

    fn try_from(value: InputEvent) -> Result<Self, Self::Error> {
        match value.change {
            InputChange::XkbLayout | InputChange::XkbKeymap => {
                if let Some(layout) = value.input.xkb_active_layout_name {
                    Ok(KeyboardLayoutUpdate(layout))
                } else {
                    Err(())
                }
            }
            _ => Err(()),
        }
    }
}

#[cfg(all(test, feature = "keyboard+sway"))]
mod keyboard_tests {
    use super::*;
    use crate::clients::sway::Input;

    #[test]
    fn empty_input_snapshot_does_not_fabricate_an_empty_layout() {
        assert!(keyboard_layout_from_inputs(&[]).is_none());
        assert!(
            keyboard_layout_from_inputs(&[Input {
                identifier: "pointer".to_string(),
                xkb_active_layout_name: None,
            }])
            .is_none()
        );
        assert_eq!(
            keyboard_layout_from_inputs(&[Input {
                identifier: "keyboard".to_string(),
                xkb_active_layout_name: Some("English (US)".to_string()),
            }])
            .expect("keyboard layout should be selected")
            .0,
            "English (US)"
        );
    }
}

#[cfg(feature = "bindmode+sway")]
use super::{BindModeClient, BindModeUpdate};

#[cfg(feature = "bindmode+sway")]
impl BindModeClient for Client {
    fn subscribe(&self) -> super::Result<Receiver<BindModeUpdate>> {
        let (tx, rx) = channel(16);
        let alive_tx = tx.clone();

        await_sync(async {
            self.add_mode_listener(
                move || alive_tx.receiver_count() > 0,
                move |message| {
                    let mode = match message {
                        ModeMessage::Snapshot(mode) | ModeMessage::Event(mode) => mode,
                    };
                    tracing::trace!("mode: {:?}", mode);

                    // when no binding is active the bindmode is named "default", but we must display
                    // nothing in this case.
                    let name = if mode.change == "default" {
                        String::new()
                    } else {
                        mode.change.clone()
                    };

                    tx.send(BindModeUpdate {
                        name,
                        pango_markup: mode.pango_markup,
                    })
                    .is_ok()
                },
            )
            .await
        })
        .map_err(|err| super::Error::Other(err.into()))?;

        Ok(rx)
    }
}
