//! Generic freedesktop application discovery for the optional built-in local provider.
//!
//! Configured command providers remain the public extension point. `cbar:desktop-files` exists so
//! a plain Linux desktop can discover its own applications without Nix, SSH or a fleet helper.

use std::collections::HashSet;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cbar_launcher_core::config::MAX_INVENTORY_APPS;
use walkdir::WalkDir;

const MAX_DESKTOP_FILE_BYTES: u64 = 1024 * 1024;
const MAX_DESKTOP_WALK_ENTRIES: usize = 50_000;

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    id: String,
    name: String,
    icon: String,
    exec: String,
    terminal: bool,
    desktop_file: String,
}

pub async fn inventory(
    machine: &str,
    max_bytes: usize,
    max_apps: usize,
) -> Result<Vec<u8>, String> {
    let machine = machine.to_string();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut guard = CancelOnDrop {
        cancelled: cancelled.clone(),
        armed: true,
    };
    let result = tokio::task::spawn_blocking(move || {
        inventory_blocking(&machine, &cancelled, max_bytes, max_apps)
    })
    .await
    .map_err(|error| format!("desktop inventory worker failed: {error}"))?;
    guard.disarm();
    result
}

struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

fn inventory_blocking(
    machine: &str,
    cancelled: &AtomicBool,
    max_bytes: usize,
    max_apps: usize,
) -> Result<Vec<u8>, String> {
    if cancelled.load(Ordering::Acquire) {
        return Err("desktop inventory cancelled".to_string());
    }
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut encoded_bytes = 1_024usize;
    let current_desktops = current_desktops();
    for directory in application_dirs() {
        scan_entries(
            &directory,
            WalkDir::new(&directory).follow_links(false),
            &current_desktops,
            cancelled,
            &mut seen,
            &mut entries,
            max_bytes,
            max_apps,
            &mut encoded_bytes,
        )?;
    }
    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    let applications = entries
        .into_iter()
        .map(|entry| {
            serde_json::json!({
                "id": entry.id,
                "name": entry.name,
                "icon": entry.icon,
                "exec": entry.exec,
                "terminal": entry.terminal,
                "desktop_file": entry.desktop_file,
            })
        })
        .collect::<Vec<_>>();
    let encoded = serde_json::to_vec(&serde_json::json!({
        "host": machine,
        "error": null,
        "folders": [{ "label": "Other", "apps": applications }],
    }))
    .map_err(|error| format!("unable to encode desktop inventory: {error}"))?;
    if encoded.len() > max_bytes {
        return Err(format!(
            "desktop inventory exceeded this provider's fair-share limit of {max_bytes} bytes"
        ));
    }
    Ok(encoded)
}

// These are independent scan bounds/counters rather than one hidden mutable context; keeping each
// visible at the call site makes the cancellation and cumulative-budget contract auditable.
#[allow(clippy::too_many_arguments)]
fn scan_entries<I>(
    directory: &Path,
    walk: I,
    current_desktops: &HashSet<String>,
    cancelled: &AtomicBool,
    seen: &mut HashSet<String>,
    entries: &mut Vec<Entry>,
    max_bytes: usize,
    max_apps: usize,
    encoded_bytes: &mut usize,
) -> Result<(), String>
where
    I: IntoIterator<Item = Result<walkdir::DirEntry, walkdir::Error>>,
{
    for entry in walk.into_iter().take(MAX_DESKTOP_WALK_ENTRIES) {
        // This check deliberately precedes error/file/extension filtering. A cancelled walk through
        // a huge tree of non-desktop entries must stop just as promptly as one finding apps.
        if cancelled.load(Ordering::Acquire) {
            return Err("desktop inventory cancelled".to_string());
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entries.len() >= max_apps.min(MAX_INVENTORY_APPS) {
            break;
        }
        if !(entry.file_type().is_file() || entry.file_type().is_symlink())
            || !entry.path().extension().is_some_and(|ext| ext == "desktop")
        {
            continue;
        }
        let path = entry.path();
        // The freedesktop desktop-file id is the path relative to `applications`, with path
        // separators mapped to dashes. Basename-only ids collapse legitimate nested entries.
        let Some(id) = desktop_id(directory, path) else {
            continue;
        };
        // XDG_DATA_HOME precedes XDG_DATA_DIRS. First desktop id wins that precedence.
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(entry) = parse_entry(&id, path, current_desktops, cancelled) {
            let entry_bytes = encoded_entry_bytes(&entry)?;
            *encoded_bytes = encoded_bytes.saturating_add(entry_bytes);
            if *encoded_bytes > max_bytes {
                return Err(format!(
                    "desktop inventory exceeded this provider's fair-share limit of {max_bytes} bytes"
                ));
            }
            entries.push(entry);
        }
    }
    Ok(())
}

fn encoded_entry_bytes(entry: &Entry) -> Result<usize, String> {
    // Exact JSON escaping for every retained field plus a fixed allowance for keys/punctuation.
    // This is calculated before storing the entry, so thousands of individually valid desktop
    // files cannot accumulate an unbounded intermediate tree before final serialization.
    [
        entry.id.as_str(),
        entry.name.as_str(),
        entry.icon.as_str(),
        entry.exec.as_str(),
        entry.desktop_file.as_str(),
    ]
    .into_iter()
    .try_fold(128usize, |total, field| {
        serde_json::to_string(field)
            .map(|encoded| total.saturating_add(encoded.len()))
            .map_err(|error| format!("unable to measure desktop inventory field: {error}"))
    })
}

fn desktop_id(directory: &Path, path: &Path) -> Option<String> {
    let id = path
        .strip_prefix(directory)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("-");
    (!id.is_empty()).then_some(id)
}

fn application_dirs() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
    {
        roots.push(home.join("applications"));
    }
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| {
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        });
    roots.extend(system.into_iter().map(|root| root.join("applications")));
    roots.into_iter().filter(|path| path.is_dir()).collect()
}

fn current_desktops() -> HashSet<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_entry(
    id: &str,
    path: &Path,
    current_desktops: &HashSet<String>,
    cancelled: &AtomicBool,
) -> Option<Entry> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    // Packaged symlinks are valid desktop entries, but a FIFO/device placed in a writable XDG
    // applications directory must not pin this provider's blocking worker before fstat rejects it.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_DESKTOP_FILE_BYTES {
        return None;
    }
    let mut text = String::with_capacity(metadata.len() as usize);
    file.take(MAX_DESKTOP_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_DESKTOP_FILE_BYTES {
        return None;
    }
    let mut in_desktop_entry = false;
    let mut name = None;
    let mut exact_name = None;
    let mut language_name = None;
    let mut icon = String::new();
    let mut exec = None;
    let mut try_exec = None;
    let mut terminal = false;
    let mut kind = None;
    let mut hidden = false;
    let mut no_display = false;
    let mut only_show_in = None;
    let mut not_show_in = None;
    let locale = std::env::var("LC_MESSAGES")
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    let locale = locale.split('.').next().unwrap_or_default();
    let language = locale.split('_').next().unwrap_or_default();
    let exact_name_key = format!("Name[{locale}]");
    let language_name_key = format!("Name[{language}]");

    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Type" => kind = Some(value),
            "Name" => name = Some(value.to_string()),
            key if key == exact_name_key => exact_name = Some(value.to_string()),
            key if key == language_name_key => language_name = Some(value.to_string()),
            "Icon" => icon = value.to_string(),
            "Exec" => exec = Some(value.to_string()),
            "TryExec" => try_exec = Some(value.trim().to_string()),
            "Terminal" => terminal = value.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            "OnlyShowIn" => only_show_in = Some(desktop_set(value)),
            "NotShowIn" => not_show_in = Some(desktop_set(value)),
            _ => {}
        }
    }
    if hidden || no_display || kind != Some("Application") {
        return None;
    }
    if only_show_in
        .as_ref()
        .is_some_and(|allowed| allowed.is_disjoint(current_desktops))
        || not_show_in
            .as_ref()
            .is_some_and(|blocked| !blocked.is_disjoint(current_desktops))
        || try_exec
            .as_deref()
            .is_some_and(|name| !executable_exists(name))
    {
        return None;
    }
    let name = exact_name.or(language_name).or(name)?.trim().to_string();
    let exec = exec?.trim().to_string();
    if name.is_empty()
        || exec.is_empty()
        || id.len() > cbar_launcher_core::config::MAX_INVENTORY_APP_TEXT_BYTES
        || name.len() > cbar_launcher_core::config::MAX_INVENTORY_APP_TEXT_BYTES
        || icon.len() > cbar_launcher_core::config::MAX_INVENTORY_APP_TEXT_BYTES
        || exec.len() > cbar_launcher_core::config::MAX_INVENTORY_EXEC_BYTES
        || path.as_os_str().as_bytes().len()
            > cbar_launcher_core::config::MAX_INVENTORY_APP_TEXT_BYTES
    {
        return None;
    }
    Some(Entry {
        id: id.to_string(),
        name,
        icon,
        exec,
        terminal,
        desktop_file: path.to_string_lossy().to_string(),
    })
}

fn desktop_set(value: &str) -> HashSet<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn executable_exists(name: &str) -> bool {
    let executable = |path: &Path| {
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    let path = Path::new(name);
    if path.is_absolute() || name.contains('/') {
        return executable(path);
    }
    std::env::var_os("PATH")
        .map(|value| {
            std::env::split_paths(&value).any(|directory| executable(&directory.join(name)))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn parse_for_test(id: &str, path: &Path, desktops: &HashSet<String>) -> Option<Entry> {
        parse_entry(id, path, desktops, &AtomicBool::new(false))
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "cbar-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create unique test directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn desktop_entry_contract_preserves_argv_source_and_terminal_flag() {
        let root = TempDir::new("desktop-entry");
        let path = root.0.join("org.example.Editor.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Editor\nIcon=editor\nExec=editor --new %U\nTerminal=true\n",
        )
        .expect("desktop fixture");
        let entry = parse_for_test("org.example.Editor.desktop", &path, &HashSet::new())
            .expect("visible application");
        assert_eq!(entry.name, "Editor");
        assert_eq!(entry.exec, "editor --new %U");
        assert!(entry.terminal);
    }

    #[test]
    fn nested_desktop_ids_use_the_freedesktop_relative_path() {
        let root = Path::new("/data/applications");
        assert_eq!(
            desktop_id(root, Path::new("/data/applications/vendor/tool.desktop")),
            Some("vendor-tool.desktop".into())
        );
    }

    #[test]
    fn hidden_and_non_application_entries_do_not_leak_into_inventory() {
        let root = TempDir::new("hidden-entry");
        let path = root.0.join("hidden.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Hidden\nExec=hidden\nNoDisplay=true\n",
        )
        .expect("desktop fixture");
        assert!(parse_for_test("hidden.desktop", &path, &HashSet::new()).is_none());
    }

    #[test]
    fn packaged_desktop_entry_symlinks_are_supported() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new("desktop-symlink");
        let target = root.0.join("target");
        let link = root.0.join("org.example.Linked.desktop");
        std::fs::write(
            &target,
            "[Desktop Entry]\nType=Application\nName=Linked\nExec=linked\n",
        )
        .unwrap();
        symlink(&target, &link).unwrap();
        assert!(parse_for_test("org.example.Linked.desktop", &link, &HashSet::new()).is_some());
    }

    #[test]
    fn desktop_entry_fifo_cannot_pin_the_provider_worker() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let root = TempDir::new("desktop-fifo");
        let path = root.0.join("blocked.desktop");
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid, NUL-terminated path and the temporary directory is private
        // to this test.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let worker_path = path.clone();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let parsed = parse_for_test("blocked.desktop", &worker_path, &HashSet::new());
            let _ = sent.send(parsed);
        });
        match received.recv_timeout(Duration::from_millis(250)) {
            Ok(parsed) => assert!(parsed.is_none()),
            Err(error) => {
                // Make a missing O_NONBLOCK regression recoverable rather than hanging the test
                // process forever: a writer releases the worker's blocking read-open before fail.
                let _writer = std::fs::OpenOptions::new().write(true).open(&path).ok();
                let _ = worker.join();
                panic!("desktop FIFO read did not remain nonblocking: {error}");
            }
        }
        worker.join().unwrap();
    }

    #[test]
    fn desktop_visibility_and_try_exec_are_honoured() {
        let root = TempDir::new("desktop-visibility");
        let executable = root.0.join("available");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.0.join("visible.desktop");
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=Visible\nExec=visible\nTryExec={}\nOnlyShowIn=GNOME;KDE;\nNotShowIn=XFCE;\n",
                executable.display()
            ),
        )
        .unwrap();

        assert!(
            parse_for_test("visible.desktop", &path, &HashSet::from(["GNOME".into()])).is_some()
        );
        assert!(
            parse_for_test("visible.desktop", &path, &HashSet::from(["XFCE".into()])).is_none()
        );
        assert!(parse_for_test("visible.desktop", &path, &HashSet::new()).is_none());
    }

    #[test]
    fn blocking_desktop_scan_observes_cancellation() {
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            inventory_blocking("local", &cancelled, 16 * 1024 * 1024, MAX_INVENTORY_APPS)
                .unwrap_err(),
            "desktop inventory cancelled"
        );
    }

    #[test]
    fn desktop_scan_cancels_mid_walk_before_filtering_non_desktop_entries() {
        struct CancelAfterFirst<I> {
            inner: I,
            cancelled: Arc<AtomicBool>,
            yielded: bool,
        }

        impl<I: Iterator> Iterator for CancelAfterFirst<I> {
            type Item = I::Item;

            fn next(&mut self) -> Option<Self::Item> {
                if self.yielded {
                    self.cancelled.store(true, Ordering::Release);
                }
                let next = self.inner.next();
                self.yielded |= next.is_some();
                next
            }
        }

        let root = TempDir::new("desktop-mid-walk-cancel");
        std::fs::write(root.0.join("not-an-application.txt"), "fixture").unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let walk = CancelAfterFirst {
            inner: WalkDir::new(&root.0).into_iter(),
            cancelled: cancelled.clone(),
            yielded: false,
        };
        let mut encoded_bytes = 1_024;
        let error = scan_entries(
            &root.0,
            walk,
            &HashSet::new(),
            &cancelled,
            &mut HashSet::new(),
            &mut Vec::new(),
            16 * 1024 * 1024,
            MAX_INVENTORY_APPS,
            &mut encoded_bytes,
        )
        .unwrap_err();
        assert_eq!(error, "desktop inventory cancelled");
    }

    #[test]
    fn many_individually_valid_desktop_entries_obey_cumulative_budget() {
        let root = TempDir::new("desktop-cumulative-budget");
        let payload = "x".repeat(256);
        for index in 0..20 {
            std::fs::write(
                root.0.join(format!("app-{index}.desktop")),
                format!(
                    "[Desktop Entry]\nType=Application\nName={payload}{index}\nExec=program {payload}\n"
                ),
            )
            .unwrap();
        }
        let mut entries = Vec::new();
        let mut bytes = 1_024;
        let error = scan_entries(
            &root.0,
            WalkDir::new(&root.0),
            &HashSet::new(),
            &AtomicBool::new(false),
            &mut HashSet::new(),
            &mut entries,
            2_000,
            MAX_INVENTORY_APPS,
            &mut bytes,
        )
        .unwrap_err();
        assert!(error.contains("fair-share limit"), "{error}");
        assert!(entries.len() < 20, "the intermediate tree stayed bounded");
    }
}
