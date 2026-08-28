//! Toolkit-free glue between independently arriving inventories and cbar's launcher model.

use std::collections::HashMap;
#[cfg(test)]
use std::sync::Arc;

use cbar_launcher_core::config;
use cbar_launcher_core::model::{App, Focus, Line, Machine, State};
#[cfg(test)]
use cbar_launcher_core::usage;

#[cfg(test)]
use super::provider::ProviderStatus;
use super::provider::ProviderUpdate;

#[cfg(test)]
pub struct LauncherModel {
    pub config: config::Config,
    pub state: State,
    statuses: HashMap<String, ProviderStatus>,
}

/// Merge every provider state ready in one GTK tick with one model clone/rebuild/render. Slot
/// equality is checked before cloning the rest of the fleet, so an unchanged refresh costs O(1)
/// model work instead of duplicating every independently retained inventory.
pub(crate) fn apply_updates_to_state<'a>(
    config: &config::Config,
    state: &mut State,
    updates: impl IntoIterator<Item = &'a ProviderUpdate>,
) -> bool {
    let mut replacements = HashMap::new();
    for update in updates {
        let Some(index) = config
            .machines
            .iter()
            .position(|machine| machine.name == update.machine)
        else {
            continue;
        };
        if let Some(column) = update.column.as_deref() {
            if state.base.get(index) == Some(column) {
                continue;
            }
            replacements.insert(index, column.clone());
        } else {
            let mut machine = empty_machine(&config.machines[index], state.folders.len());
            machine.error = update.error.clone();
            if state.base.get(index) != Some(&machine) {
                replacements.insert(index, machine);
            }
        }
    }
    if replacements.is_empty() {
        return false;
    }
    let anchor = SelectionAnchor::capture(state);
    let mut base = state.base.clone();
    for (index, machine) in replacements {
        if let Some(slot) = base.get_mut(index) {
            *slot = machine;
        }
    }
    let changed = state.replace_inventory(state.folders.clone(), config.layout.clone(), base);
    if changed {
        state.migrate_names_to_ids();
        anchor.restore(state);
    }
    changed
}

#[cfg(test)]
impl LauncherModel {
    fn from_parts(
        config: config::Config,
        placement: cbar_launcher_core::model::Placement,
        placement_writable: bool,
        visibility: cbar_launcher_core::model::Visibility,
        visibility_writable: bool,
        loaded_usage: usage::Usage,
        usage_writable: bool,
    ) -> Self {
        let folders = config.folder_rows();
        let base = config
            .machines
            .iter()
            .map(|machine| empty_machine(machine, folders.len()))
            .collect();
        let mut state = State {
            folders,
            layout: config.layout.clone(),
            usage: loaded_usage,
            usage_writable,
            z: 2.0,
            half_life_days: usage::HALF_LIFE_DAYS,
            base,
            placement,
            placement_writable,
            visibility,
            visibility_writable,
            machines: Vec::new(),
            view: Vec::new(),
            col: 0,
            row: 0,
            line: 0,
            item: 0,
            item_goal: 0,
            focus: Focus::Outside,
            query: String::new(),
        };
        state.rebuild();
        Self {
            config,
            state,
            statuses: HashMap::new(),
        }
    }

    fn new_for_test(config: config::Config) -> Self {
        Self::from_parts(
            config,
            Default::default(),
            false,
            Default::default(),
            false,
            Default::default(),
            false,
        )
    }

    /// Merge one provider result in its configured column while retaining the user's context.
    pub fn apply(&mut self, update: ProviderUpdate) -> bool {
        let Some(index) = self
            .config
            .machines
            .iter()
            .position(|machine| machine.name == update.machine)
        else {
            return false;
        };

        let anchor = SelectionAnchor::capture(&self.state);
        let machine = update.column.as_deref().cloned().unwrap_or_else(|| {
            let mut machine = empty_machine(&self.config.machines[index], self.state.folders.len());
            machine.error = update.error.clone();
            machine
        });
        let mut base = self.state.base.clone();
        if base.get(index) == Some(&machine)
            && self.statuses.get(&update.machine) == Some(&update.status)
        {
            return false;
        }
        if let Some(slot) = base.get_mut(index) {
            *slot = machine;
        }
        self.statuses.insert(update.machine, update.status);
        let changed = self.state.replace_inventory(
            self.state.folders.clone(),
            self.config.layout.clone(),
            base,
        );
        if changed {
            // Old placement files keyed on display names can only be migrated after the first real
            // inventory for that machine arrives.
            self.state.migrate_names_to_ids();
            anchor.restore(&mut self.state);
        }
        changed
    }
}

fn empty_machine(config: &config::MachineConfig, rows: usize) -> Machine {
    Machine {
        name: config.name.clone(),
        aliases: config.aliases.clone(),
        accent: config.accent.clone(),
        launch: config.launch.clone(),
        error: None,
        cells: vec![Vec::new(); rows],
    }
}

/// Parse one provider's answer without involving any other provider.
pub(crate) fn machine_from(
    machine: &config::MachineConfig,
    inventory: Option<&config::Inventory>,
    provider_error: Option<&str>,
    rows: &[String],
    subrows: &HashMap<String, Vec<config::SubRow>>,
) -> Machine {
    let mut result = empty_machine(machine, rows.len());
    let Some(inventory) = inventory else {
        result.error = provider_error.map(ToString::to_string);
        return result;
    };

    for folder in &inventory.folders {
        let declared = subrows.get(&folder.label);
        let row_label = |app: &config::InventoryApp| -> String {
            let id = app.id.clone().unwrap_or_default().to_lowercase();
            let name = app.name.to_lowercase();
            declared
                .into_iter()
                .flatten()
                .find(|subrow| {
                    subrow.apps.iter().any(|wanted| {
                        let wanted = wanted.to_lowercase();
                        !wanted.is_empty() && (id.contains(&wanted) || name.contains(&wanted))
                    })
                })
                .map(|subrow| format!("{}/{}", folder.label, subrow.name))
                .unwrap_or_else(|| folder.label.clone())
        };

        let mut by_row: Vec<Vec<&config::InventoryApp>> = vec![Vec::new(); rows.len()];
        for app in &folder.apps {
            let label = row_label(app);
            let row = rows
                .iter()
                .position(|candidate| *candidate == label)
                .unwrap_or(rows.len().saturating_sub(1));
            if let Some(bucket) = by_row.get_mut(row) {
                bucket.push(app);
            }
        }
        for (row, apps) in by_row
            .into_iter()
            .enumerate()
            .filter(|(_, apps)| !apps.is_empty())
        {
            result.cells[row].push(Line {
                name: None,
                apps: apps
                    .into_iter()
                    .map(|app| {
                        std::sync::Arc::new(App {
                            id: app.id.clone().unwrap_or_else(|| app.name.clone()),
                            name: app.name.clone(),
                            icon: app.icon.clone(),
                            exec: app.exec.clone(),
                            terminal: app.terminal,
                            desktop_file: app.desktop_file.clone(),
                        })
                    })
                    .collect(),
            });
        }
    }
    // A cached inventory remains fully populated; the provider's fresh failure is represented on
    // that same column instead of replacing it with an empty one.
    result.error = provider_error
        .map(ToString::to_string)
        .or_else(|| inventory.error.clone());
    result
}

#[derive(Clone)]
pub(crate) struct SelectionAnchor {
    machine: Option<String>,
    folder: Option<String>,
    app: Option<String>,
    focus: Focus,
}

impl SelectionAnchor {
    pub(crate) fn capture(state: &State) -> Self {
        let machine = state
            .view
            .get(state.col)
            .map(|machine| machine.name.clone());
        let folder = state.folders.get(state.row).cloned();
        let app = state
            .view
            .get(state.col)
            .and_then(|machine| machine.cells.get(state.row))
            .and_then(|lines| lines.get(state.line))
            .and_then(|line| line.apps.get(state.item))
            .map(|app| app.id.clone());
        Self {
            machine,
            folder,
            app,
            focus: state.focus,
        }
    }

    pub(crate) fn restore(self, state: &mut State) {
        if let Some(machine) = &self.machine
            && let Some(column) = state.view.iter().position(|item| item.name == *machine)
        {
            state.col = column;
        }
        if let Some(app) = &self.app
            && let Some((row, line, item)) = state
                .view
                .get(state.col)
                .into_iter()
                .flat_map(|machine| machine.cells.iter().enumerate())
                .flat_map(|(row, lines)| {
                    lines
                        .iter()
                        .enumerate()
                        .map(move |(line, apps)| (row, line, apps))
                })
                .find_map(|(row, line, apps)| {
                    apps.apps
                        .iter()
                        .position(|candidate| candidate.id == *app)
                        .map(|item| (row, line, item))
                })
        {
            state.row = row;
            state.line = line;
            state.item = item;
            state.item_goal = item;
            state.focus = self.focus;
            return;
        }
        if let Some(folder) = &self.folder
            && let Some(row) = state.folders.iter().position(|item| item == folder)
        {
            state.row = row;
        }
        state.focus = self.focus;
        state.clamp();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> config::Config {
        serde_json::from_str(
            r##"{
                "machines": [
                    {"name":"local","inventory":["local"],"launch":["{}"]},
                    {"name":"remote","inventory":["remote"],"launch":["ssh","remote","{}"]}
                ],
                "folders":["Editors"]
            }"##,
        )
        .expect("valid launcher config")
    }

    fn inventory(host: &str, apps: &[(&str, &str)]) -> Vec<u8> {
        let apps = apps
            .iter()
            .map(|(id, name)| {
                format!(
                    r#"{{"id":"{id}","name":"{name}","exec":"{id}","icon":"","terminal":false}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"host":"{host}","folders":[{{"label":"Editors","apps":[{apps}]}}]}}"#)
            .into_bytes()
    }

    fn update(machine: &str, inventory: Vec<u8>) -> ProviderUpdate {
        let config = config();
        let rows = config.folder_rows();
        let inventory =
            cbar_launcher_core::config::parse_inventory(&inventory).expect("valid test inventory");
        let machine_config = config
            .machines
            .iter()
            .find(|candidate| candidate.name == machine)
            .expect("configured test machine");
        ProviderUpdate {
            machine: machine.into(),
            column: Some(Arc::new(machine_from(
                machine_config,
                Some(&inventory),
                None,
                &rows,
                &config.subrows,
            ))),
            status: ProviderStatus::Online,
            error: None,
        }
    }

    #[test]
    fn independently_arriving_columns_keep_configured_order() {
        let mut model = LauncherModel::new_for_test(config());
        model.apply(update("remote", inventory("remote", &[("zed", "Zed")])));
        assert_eq!(
            model
                .state
                .view
                .iter()
                .map(|machine| machine.name.as_str())
                .collect::<Vec<_>>(),
            ["local", "remote"]
        );
        assert!(model.state.view[0].cells.iter().all(Vec::is_empty));
        assert!(!model.state.view[1].cells[0].is_empty());
    }

    #[test]
    fn streaming_refresh_preserves_query_focus_and_selected_identity() {
        let mut model = LauncherModel::new_for_test(config());
        model.apply(update(
            "local",
            inventory("local", &[("code", "Code"), ("helix", "Helix")]),
        ));
        model.state.set_query("hel".into());
        model.state.focus = Focus::Inside;
        model.state.col = 0;
        model.state.row = 0;
        model.state.line = 0;
        model.state.item = 0;

        model.apply(update(
            "remote",
            inventory("remote", &[("helix-remote", "Helix")]),
        ));
        assert_eq!(model.state.query, "hel");
        assert_eq!(model.state.focus, Focus::Inside);
        let selected = &model.state.view[model.state.col].cells[model.state.row][model.state.line]
            .apps[model.state.item];
        assert_eq!(selected.id, "helix");
        assert_eq!(model.state.view[model.state.col].name, "local");
    }

    #[test]
    fn offline_update_keeps_last_known_apps_and_marks_only_that_column() {
        let mut model = LauncherModel::new_for_test(config());
        let bytes = inventory("remote", &[("zed", "Zed")]);
        model.apply(update("remote", bytes.clone()));
        let mut column = update("remote", bytes)
            .column
            .expect("normalized test column")
            .as_ref()
            .clone();
        column.error = Some("network unreachable".into());
        model.apply(ProviderUpdate {
            machine: "remote".into(),
            column: Some(Arc::new(column)),
            status: ProviderStatus::Offline,
            error: Some("network unreachable".into()),
        });
        assert_eq!(
            model.state.base[1].error.as_deref(),
            Some("network unreachable")
        );
        assert_eq!(model.state.base[1].cells[0][0].apps[0].id, "zed");
        assert!(model.state.base[0].error.is_none());
    }

    #[test]
    fn large_valid_columns_remain_independent_in_any_arrival_order() {
        fn large(host: &str, prefix: &str, count: usize) -> Vec<u8> {
            let apps = (0..count)
                .map(|index| (format!("{prefix}-{index}"), format!("{prefix} {index}")))
                .collect::<Vec<_>>();
            let pairs = apps
                .iter()
                .map(|(id, name)| (id.as_str(), name.as_str()))
                .collect::<Vec<_>>();
            inventory(host, &pairs)
        }
        let count_apps = |machine: &Machine| {
            machine
                .cells
                .iter()
                .flatten()
                .map(|line| line.apps.len())
                .sum::<usize>()
        };

        for order in [["remote", "local"], ["local", "remote"]] {
            let mut model = LauncherModel::new_for_test(config());
            for machine in order {
                model.apply(update(machine, large(machine, machine, 3_000)));
            }
            assert_eq!(count_apps(&model.state.base[0]), 3_000);
            assert_eq!(count_apps(&model.state.base[1]), 3_000);
            assert!(
                model
                    .state
                    .base
                    .iter()
                    .all(|machine| machine.error.is_none())
            );
        }
    }
}
