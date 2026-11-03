//! The daemon's profile: which set of folders and Credential Manager names a
//! daemon uses, decided by its token's integrity level.
//!
//! An ordinary daemon (below High integrity) keeps today's folders: config in
//! the roaming config dir, binaries, worktrees, `daemon-id`, tailnet and
//! summarizer under the machine-local data dir. An elevated daemon (at or above
//! High, the rule `daemon::integrity::IntegrityLevel::is_elevated` applies too)
//! keeps all of them under its own protected root, `<data_local>/elevated`
//! (or the config override itself when one is set), and files its Credential
//! Manager entries under a suffixed service name. An override variable beats
//! the profile for the folder it names.

use anyhow::{Context as _, anyhow};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `SECURITY_MANDATORY_HIGH_RID`: the lowest integrity RID that is elevated.
const HIGH_RID: u32 = 0x3000;

/// Which set of folders and service names a daemon uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Below High integrity: today's folders and names.
    Ordinary,
    /// At or above High integrity: the protected root and suffixed names.
    Elevated,
}

/// The profile of a token whose integrity label ends in `rid`: elevated from
/// High (`0x3000`) up, ordinary below (Medium Plus included).
#[must_use]
pub fn profile_for_rid(rid: u32) -> Profile {
    if rid >= HIGH_RID {
        Profile::Elevated
    } else {
        Profile::Ordinary
    }
}

/// This process's profile, read from its token once and cached.
///
/// # Errors
///
/// The token's integrity level could not be read. The caller fails rather
/// than guess a profile.
pub fn own_profile() -> anyhow::Result<Profile> {
    static OWN: OnceLock<Profile> = OnceLock::new();
    if let Some(profile) = OWN.get() {
        return Ok(*profile);
    }
    let profile = read_own_profile()?;
    Ok(*OWN.get_or_init(|| profile))
}

/// The profile of the running process `pid`, read from its token's integrity
/// level.
///
/// # Errors
///
/// The process could not be opened for a limited query, or its token or
/// integrity label could not be read.
#[cfg(windows)]
pub fn profile_of_process(pid: u32) -> anyhow::Result<Profile> {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: OpenProcess takes no pointers; the handle it returns is owned
    // by `Handle` and closed once.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .with_context(|| format!("opening process {pid}"))?;
    let process = Handle(process);
    profile_of_token_of(process.0).with_context(|| format!("reading process {pid}'s integrity"))
}

/// The profile of the running process `pid`: always ordinary, since other
/// platforms have no integrity levels.
///
/// # Errors
///
/// None on this platform; the Windows read can fail.
#[cfg(not(windows))]
pub fn profile_of_process(pid: u32) -> anyhow::Result<Profile> {
    let _ = pid;
    Ok(Profile::Ordinary)
}

#[cfg(windows)]
fn read_own_profile() -> anyhow::Result<Profile> {
    use windows::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: the current-process pseudo-handle needs no closing.
    profile_of_token_of(unsafe { GetCurrentProcess() }).context("reading this process's integrity")
}

/// A handle closed when dropped.
#[cfg(windows)]
struct Handle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for Handle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;

        // SAFETY: the handle was opened by this module and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The profile of the token of `process`, a handle opened with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION` (or the current-process pseudo-handle).
#[cfg(windows)]
fn profile_of_token_of(process: windows::Win32::Foundation::HANDLE) -> anyhow::Result<Profile> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows::Win32::System::Threading::OpenProcessToken;

    let mut token = HANDLE::default();
    // SAFETY: `process` is a live process handle the caller holds; `token`
    // receives a handle closed by `Handle`.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }
        .context("opening the process's token")?;
    let token = Handle(token);
    let mut needed = 0u32;
    // SAFETY: a size query with no buffer; it fails with
    // ERROR_INSUFFICIENT_BUFFER and sets `needed`.
    let _ = unsafe { GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &raw mut needed) };
    if needed == 0 {
        return Err(anyhow!("the process's token reported no integrity label"));
    }
    // u64 words keep the buffer aligned for TOKEN_MANDATORY_LABEL.
    let words = usize::try_from(needed).unwrap_or(usize::MAX).div_ceil(8);
    let mut buffer = vec![0u64; words];
    // SAFETY: `buffer` holds at least `needed` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &raw mut needed,
        )
    }
    .context("reading the process's integrity label")?;
    // SAFETY: the call filled `buffer` with a TOKEN_MANDATORY_LABEL whose SID
    // points into the same buffer, which is alive and aligned.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    // SAFETY: `sid` is the valid label SID read above.
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    let Some(last) = count.checked_sub(1) else {
        return Err(anyhow!(
            "the process's integrity label has no sub-authority"
        ));
    };
    // SAFETY: `last` is below the SID's sub-authority count.
    let rid = unsafe { *GetSidSubAuthority(sid, u32::from(last)) };
    Ok(profile_for_rid(rid))
}

#[cfg(not(windows))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Windows read can fail; both share one signature"
)]
fn read_own_profile() -> anyhow::Result<Profile> {
    Ok(Profile::Ordinary)
}

/// Every folder a daemon of one profile uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRoots {
    /// The protected root an elevated daemon keeps everything under; `None`
    /// for an ordinary daemon.
    pub root: Option<PathBuf>,
    /// The config dir: `state.json`, `daemon.json`, sessions, logs.
    pub config: PathBuf,
    /// The content-addressed binary cache.
    pub binaries: PathBuf,
    /// The worktrees root.
    pub worktrees: PathBuf,
    /// The daemon's stable id file.
    pub daemon_id: PathBuf,
    /// The tailnet listener's certificate dir.
    pub tailnet: PathBuf,
    /// The summarizer's working folder.
    pub summarizer: PathBuf,
}

/// The folder overrides the environment sets; each beats the profile for the
/// folder it names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `RUSTLING_TULIP_CONFIG_DIR`, made absolute against the current dir.
    pub config: Option<PathBuf>,
    /// `RUSTLING_TULIP_BINARIES_DIR`.
    pub binaries: Option<PathBuf>,
    /// `RUSTLING_TULIP_WORKTREES_DIR`.
    pub worktrees: Option<PathBuf>,
    /// `RUSTLING_TULIP_TAILNET_DIR`.
    pub tailnet: Option<PathBuf>,
}

impl Overrides {
    /// The overrides this process's environment sets. An empty value counts as
    /// unset.
    ///
    /// # Errors
    ///
    /// `RUSTLING_TULIP_CONFIG_DIR` could not be made absolute.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// The overrides `lookup` names, by variable name.
    fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<Self> {
        let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
        // Tracers carry the config dir as their owner, and a relative one
        // would name a different dir from their cwd.
        let config = set("RUSTLING_TULIP_CONFIG_DIR")
            .map(|value| {
                std::path::absolute(&value).with_context(|| {
                    format!(
                        "resolving RUSTLING_TULIP_CONFIG_DIR={} to an absolute path",
                        value.to_string_lossy()
                    )
                })
            })
            .transpose()?;
        Ok(Self {
            config,
            binaries: set("RUSTLING_TULIP_BINARIES_DIR").map(PathBuf::from),
            worktrees: set("RUSTLING_TULIP_WORKTREES_DIR").map(PathBuf::from),
            tailnet: set("RUSTLING_TULIP_TAILNET_DIR").map(PathBuf::from),
        })
    }
}

/// The folders of `profile`, given the overrides, the app's default config
/// dir and its machine-local data dir.
#[must_use]
pub fn roots_for(
    profile: Profile,
    overrides: &Overrides,
    config_default: &Path,
    data_local: &Path,
) -> ProfileRoots {
    // Under a config override an elevated daemon keeps everything inside it,
    // so an isolated elevated run never touches the real data dir.
    let root = match profile {
        Profile::Ordinary => None,
        Profile::Elevated => Some(
            overrides
                .config
                .clone()
                .unwrap_or_else(|| data_local.join("elevated")),
        ),
    };
    // Everything but the config dir sits under `base`: the data-local dir for
    // an ordinary daemon, the protected root for an elevated one.
    let base = root.clone().unwrap_or_else(|| data_local.to_path_buf());
    let config = overrides.config.clone().unwrap_or_else(|| match &root {
        Some(root) => root.join("config"),
        None => config_default.to_path_buf(),
    });
    // An overridden config dir keeps the id inside it, so an isolated daemon
    // keeps its id in its own tree.
    let daemon_id = if overrides.config.is_some() {
        config.join("daemon-id")
    } else {
        base.join("daemon-id")
    };
    ProfileRoots {
        binaries: overrides
            .binaries
            .clone()
            .unwrap_or_else(|| base.join("binaries")),
        worktrees: overrides
            .worktrees
            .clone()
            .unwrap_or_else(|| base.join("worktrees")),
        tailnet: overrides
            .tailnet
            .clone()
            .unwrap_or_else(|| base.join("tailnet")),
        summarizer: base.join("summarizer"),
        daemon_id,
        config,
        root,
    }
}

/// The profiles whose daemon a caller of profile `caller` may use, in the
/// order it looks for one.
/// An ordinary caller uses the ordinary profile only and never sees the
/// elevated one; an elevated caller first looks for an ordinary daemon of its
/// account, which gives no elevation, then uses its own profile.
#[must_use]
pub fn profiles_to_probe(caller: Profile) -> &'static [Profile] {
    match caller {
        Profile::Ordinary => &[Profile::Ordinary],
        Profile::Elevated => &[Profile::Ordinary, Profile::Elevated],
    }
}

/// The profiles [`profiles_to_probe`] lists for `caller`, each with its
/// folders, in that order. Two profiles whose config dirs are equal (both
/// resolve to one config override) are probed once, as the later profile in
/// the list: the caller's own.
#[must_use]
pub fn roots_to_probe_in(
    caller: Profile,
    overrides: &Overrides,
    config_default: &Path,
    data_local: &Path,
) -> Vec<(Profile, ProfileRoots)> {
    let mut probed: Vec<(Profile, ProfileRoots)> = Vec::with_capacity(2);
    for &profile in profiles_to_probe(caller) {
        let roots = roots_for(profile, overrides, config_default, data_local);
        probed.retain(|(_, earlier)| earlier.config != roots.config);
        probed.push((profile, roots));
    }
    probed
}

/// [`roots_to_probe_in`] for a caller of profile `caller`, under the app's
/// folders.
///
/// # Errors
///
/// The app's folders could not be resolved.
pub fn roots_to_probe(
    caller: Profile,
    overrides: &Overrides,
) -> anyhow::Result<Vec<(Profile, ProfileRoots)>> {
    let dirs = app_dirs()?;
    Ok(roots_to_probe_in(
        caller,
        overrides,
        dirs.config_dir(),
        dirs.data_local_dir(),
    ))
}

/// The app's per-user folders.
fn app_dirs() -> anyhow::Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve the app's folders"))
}

/// The folders an elevated profile protects: its root, and the config and
/// binaries overrides when they are set and not the root itself, since an
/// ordinary process could have made any of them first. None for an ordinary
/// profile, which has no root.
fn protected_folders<'a>(roots: &'a ProfileRoots, overrides: &Overrides) -> Vec<&'a Path> {
    let Some(root) = &roots.root else {
        return Vec::new();
    };
    let mut folders = vec![root.as_path()];
    if overrides.config.is_some() && !folders.contains(&roots.config.as_path()) {
        folders.push(roots.config.as_path());
    }
    if overrides.binaries.is_some() && !folders.contains(&roots.binaries.as_path()) {
        folders.push(roots.binaries.as_path());
    }
    folders
}

/// Before anything creates a folder of an elevated profile: for each protected
/// folder ([`protected_folders`]), create it when missing and take a hold on
/// it that refuses a rename or delete for the life of the process
/// ([`crate::protected_root::open_held`]); then check each folder through the
/// handle that holds it ([`crate::protected_root::check_handle`]), so the
/// folder checked is the folder held and none can be swapped in between. A
/// second call checks again through the handles the first one took. A no-op
/// for an ordinary profile.
///
/// # Errors
///
/// A folder could not be created or opened, or the first held folder the
/// check refuses.
#[cfg(windows)]
pub fn ensure_protected(roots: &ProfileRoots, overrides: &Overrides) -> anyhow::Result<()> {
    use crate::protected_root::{check_handle, create_if_missing, open_held};
    use windows::Win32::Foundation::HANDLE;

    /// Each held folder and its raw handle value, never closed.
    static HELD: std::sync::Mutex<Vec<(PathBuf, usize)>> = std::sync::Mutex::new(Vec::new());

    let folders = protected_folders(roots, overrides);
    let mut handles = Vec::with_capacity(folders.len());
    {
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for folder in &folders {
            let known = held
                .iter()
                .find(|(path, _)| path.as_path() == *folder)
                .map(|&(_, raw)| raw);
            let raw = if let Some(raw) = known {
                raw
            } else {
                create_if_missing(folder)?;
                let raw = open_held(folder)?.into_raw().0.addr();
                held.push((folder.to_path_buf(), raw));
                raw
            };
            handles.push(raw);
        }
    }
    for (folder, raw) in folders.into_iter().zip(handles) {
        check_handle(HANDLE(std::ptr::without_provenance_mut(raw)), folder)?;
    }
    Ok(())
}

/// Before anything creates a folder of an elevated profile: create or check
/// each protected folder ([`protected_folders`]). No other platform lets an
/// open handle refuse a rename, so nothing is held. A no-op for an ordinary
/// profile.
///
/// # Errors
///
/// The first folder [`crate::protected_root::create_or_check`] refuses.
#[cfg(not(windows))]
pub fn ensure_protected(roots: &ProfileRoots, overrides: &Overrides) -> anyhow::Result<()> {
    for folder in protected_folders(roots, overrides) {
        crate::protected_root::create_or_check(folder)?;
    }
    Ok(())
}

/// The Credential Manager service a daemon of `profile` files its entries
/// under: `base`, or `base:<config_hash>` under a config override, with
/// `:elevated` after `base` for an elevated daemon.
#[must_use]
pub fn credential_service(profile: Profile, base: &str, config_hash: Option<&str>) -> String {
    match (profile, config_hash) {
        (Profile::Ordinary, None) => base.to_owned(),
        (Profile::Ordinary, Some(hash)) => format!("{base}:{hash}"),
        (Profile::Elevated, None) => format!("{base}:elevated"),
        (Profile::Elevated, Some(hash)) => format!("{base}:elevated:{hash}"),
    }
}

/// This process's profile and its folders.
///
/// # Errors
///
/// The profile could not be read, an override could not be resolved, or the
/// app's folders could not be resolved.
pub fn roots() -> anyhow::Result<(Profile, ProfileRoots)> {
    let profile = own_profile()?;
    let overrides = Overrides::from_env()?;
    let dirs = app_dirs()?;
    Ok((
        profile,
        roots_for(
            profile,
            &overrides,
            dirs.config_dir(),
            dirs.data_local_dir(),
        ),
    ))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;

    fn config_default() -> PathBuf {
        std::env::temp_dir().join("rt-roaming-config")
    }

    fn data_local() -> PathBuf {
        std::env::temp_dir().join("rt-data-local")
    }

    fn roots_of(profile: Profile, overrides: &Overrides) -> ProfileRoots {
        roots_for(profile, overrides, &config_default(), &data_local())
    }

    /// Every path field of `roots`, named.
    fn fields(roots: &ProfileRoots) -> [(&'static str, &PathBuf); 6] {
        [
            ("config", &roots.config),
            ("binaries", &roots.binaries),
            ("worktrees", &roots.worktrees),
            ("daemon_id", &roots.daemon_id),
            ("tailnet", &roots.tailnet),
            ("summarizer", &roots.summarizer),
        ]
    }

    #[test]
    fn profile_for_rid_is_elevated_from_high_up() {
        for rid in [0x1000, 0x2000, 0x2100] {
            assert_eq!(profile_for_rid(rid), Profile::Ordinary, "{rid:#x}");
        }
        for rid in [0x3000, 0x4000] {
            assert_eq!(profile_for_rid(rid), Profile::Elevated, "{rid:#x}");
        }
    }

    #[test]
    fn ordinary_roots_are_exactly_todays_paths() {
        let data_local = data_local();
        assert_eq!(
            roots_of(Profile::Ordinary, &Overrides::default()),
            ProfileRoots {
                root: None,
                config: config_default(),
                binaries: data_local.join("binaries"),
                worktrees: data_local.join("worktrees"),
                daemon_id: data_local.join("daemon-id"),
                tailnet: data_local.join("tailnet"),
                summarizer: data_local.join("summarizer"),
            },
            "an ordinary daemon's saved data stays where it is"
        );
    }

    #[test]
    fn elevated_roots_are_under_the_elevated_root_and_never_equal_ordinary() {
        let none = Overrides::default();
        let root = data_local().join("elevated");
        let elevated = roots_of(Profile::Elevated, &none);
        let ordinary = roots_of(Profile::Ordinary, &none);
        assert_eq!(elevated.root.as_ref(), Some(&root));
        for ((name, path), (_, ordinary_path)) in
            fields(&elevated).into_iter().zip(fields(&ordinary))
        {
            assert!(path.starts_with(&root), "{name}: {}", path.display());
            assert_ne!(path, ordinary_path, "{name}");
        }
    }

    #[test]
    fn an_override_beats_the_profile() {
        let scratch = std::env::temp_dir().join("rt-overrides");
        let overrides = Overrides {
            config: Some(scratch.join("config")),
            binaries: Some(scratch.join("binaries")),
            worktrees: Some(scratch.join("worktrees")),
            tailnet: Some(scratch.join("tailnet")),
        };
        for profile in [Profile::Ordinary, Profile::Elevated] {
            let roots = roots_of(profile, &overrides);
            assert_eq!(roots.config, scratch.join("config"), "{profile:?}");
            assert_eq!(roots.binaries, scratch.join("binaries"), "{profile:?}");
            assert_eq!(roots.worktrees, scratch.join("worktrees"), "{profile:?}");
            assert_eq!(roots.tailnet, scratch.join("tailnet"), "{profile:?}");
            assert_eq!(
                roots.daemon_id,
                scratch.join("config").join("daemon-id"),
                "{profile:?}: an overridden config dir keeps the id inside it"
            );
        }
        let elevated = roots_of(Profile::Elevated, &overrides);
        assert_eq!(
            elevated.root,
            Some(scratch.join("config")),
            "an elevated profile under a config override is rooted in it"
        );
        assert_eq!(
            elevated.summarizer,
            scratch.join("config").join("summarizer"),
            "an elevated summarizer goes under the config override"
        );
        assert_eq!(roots_of(Profile::Ordinary, &overrides).root, None);

        // One override at a time leaves the other folders on the profile.
        let only_tailnet = Overrides {
            tailnet: Some(scratch.join("tailnet")),
            ..Overrides::default()
        };
        let ordinary = roots_of(Profile::Ordinary, &only_tailnet);
        assert_eq!(ordinary.tailnet, scratch.join("tailnet"));
        assert_eq!(ordinary.daemon_id, data_local().join("daemon-id"));
        assert_eq!(ordinary.config, config_default());
        let elevated = roots_of(Profile::Elevated, &only_tailnet);
        assert_eq!(elevated.tailnet, scratch.join("tailnet"));
        assert_eq!(
            elevated.daemon_id,
            data_local().join("elevated").join("daemon-id")
        );
    }

    #[test]
    fn an_elevated_profile_under_a_config_override_stays_inside_it() {
        let config = std::env::temp_dir()
            .join("rt-elevated-override")
            .join("config");
        let overrides = Overrides {
            config: Some(config.clone()),
            ..Overrides::default()
        };
        let roots = roots_of(Profile::Elevated, &overrides);
        assert_eq!(
            roots,
            ProfileRoots {
                root: Some(config.clone()),
                config: config.clone(),
                binaries: config.join("binaries"),
                worktrees: config.join("worktrees"),
                daemon_id: config.join("daemon-id"),
                tailnet: config.join("tailnet"),
                summarizer: config.join("summarizer"),
            },
            "an isolated elevated run keeps everything under its config override"
        );
        for (name, path) in fields(&roots) {
            assert!(
                !path.starts_with(data_local()),
                "{name} reaches the real data dir: {}",
                path.display()
            );
        }
    }

    #[test]
    fn profiles_to_probe_lists_ordinary_first_for_an_elevated_caller() {
        assert_eq!(
            profiles_to_probe(Profile::Elevated),
            &[Profile::Ordinary, Profile::Elevated]
        );
        assert_eq!(profiles_to_probe(Profile::Ordinary), &[Profile::Ordinary]);

        let configs = |caller: Profile, overrides: &Overrides| -> Vec<(Profile, PathBuf)> {
            roots_to_probe_in(caller, overrides, &config_default(), &data_local())
                .into_iter()
                .map(|(profile, roots)| (profile, roots.config))
                .collect()
        };
        let none = Overrides::default();
        assert_eq!(
            configs(Profile::Elevated, &none),
            vec![
                (Profile::Ordinary, config_default()),
                (
                    Profile::Elevated,
                    data_local().join("elevated").join("config")
                ),
            ]
        );
        assert_eq!(
            configs(Profile::Ordinary, &none),
            vec![(Profile::Ordinary, config_default())]
        );

        // Both profiles resolve to one config override: it is probed once,
        // as the caller's own profile.
        let shared = std::env::temp_dir().join("rt-probe-shared").join("config");
        let overrides = Overrides {
            config: Some(shared.clone()),
            ..Overrides::default()
        };
        assert_eq!(
            configs(Profile::Elevated, &overrides),
            vec![(Profile::Elevated, shared.clone())]
        );
        assert_eq!(
            configs(Profile::Ordinary, &overrides),
            vec![(Profile::Ordinary, shared)]
        );
    }

    #[test]
    fn empty_overrides_count_as_unset_and_a_relative_config_is_made_absolute() {
        let empty = Overrides::from_lookup(|_| Some(OsString::new())).expect("resolve");
        assert_eq!(
            empty,
            Overrides::default(),
            "an empty value counts as unset"
        );

        let relative = Overrides::from_lookup(|name| {
            (name == "RUSTLING_TULIP_CONFIG_DIR").then(|| OsString::from("rel/config"))
        })
        .expect("resolve");
        let config = relative.config.expect("the config override is set");
        assert!(config.is_absolute(), "{}", config.display());
        assert_eq!(
            config,
            std::env::current_dir()
                .expect("current dir")
                .join("rel")
                .join("config")
        );

        let absolute_dir = std::env::temp_dir().join("rt-abs-config");
        let absolute = Overrides::from_lookup(|name| {
            (name == "RUSTLING_TULIP_CONFIG_DIR").then(|| absolute_dir.clone().into_os_string())
        })
        .expect("resolve");
        assert_eq!(absolute.config, Some(absolute_dir));
    }

    #[test]
    fn credential_service_is_plain_for_ordinary_and_suffixed_for_elevated() {
        let base = "rustling-tulip";
        assert_eq!(credential_service(Profile::Ordinary, base, None), base);
        assert_eq!(
            credential_service(Profile::Ordinary, base, Some("0a1b2c3d")),
            "rustling-tulip:0a1b2c3d"
        );
        assert_eq!(
            credential_service(Profile::Elevated, base, None),
            "rustling-tulip:elevated"
        );
        assert_eq!(
            credential_service(Profile::Elevated, base, Some("0a1b2c3d")),
            "rustling-tulip:elevated:0a1b2c3d"
        );
    }
}

#[cfg(all(test, windows))]
mod guard_tests {
    use super::*;

    /// A fresh folder path for one test, cleared of a previous run's copy.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rt-profile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn an_elevated_dirs_check_refuses_a_preexisting_default_root() {
        let data_local = scratch("default-root");
        let roots = roots_for(
            Profile::Elevated,
            &Overrides::default(),
            &data_local.join("config"),
            &data_local,
        );
        let root = roots.root.clone().expect("an elevated profile has a root");
        std::fs::create_dir_all(&root).expect("create an ordinary folder at the root");
        let err = ensure_protected(&roots, &Overrides::default())
            .expect_err("an ordinary folder is no protected root");
        let text = format!("{err:#}");
        assert!(text.contains("is unsafe"), "unexpected refusal: {text}");
        let _ = std::fs::remove_dir_all(&data_local);
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn the_elevated_root_cannot_be_renamed_while_held() {
        let dir = scratch("held");
        let aside = scratch("held-aside");
        std::fs::create_dir_all(&dir).expect("create the folder");
        let held = crate::protected_root::open_held(&dir).expect("hold the folder");
        let renamed = std::fs::rename(&dir, &aside);
        drop(held);
        assert!(
            renamed.is_err(),
            "a held folder was renamed to {}",
            aside.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&aside);
    }

    /// The guard holds every folder it checks before it checks any, so a
    /// refused root still leaves the binaries override held.
    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn a_checked_override_is_held() {
        let data_local = scratch("held-override");
        let binaries = scratch("held-override-binaries");
        let aside = scratch("held-override-aside");
        let overrides = Overrides {
            binaries: Some(binaries.clone()),
            ..Overrides::default()
        };
        let roots = roots_for(
            Profile::Elevated,
            &overrides,
            &data_local.join("config"),
            &data_local,
        );
        let root = roots.root.clone().expect("an elevated profile has a root");
        assert_eq!(root, data_local.join("elevated"));
        std::fs::create_dir_all(&root).expect("create an ordinary folder at the root");
        std::fs::create_dir_all(&binaries).expect("create the binaries override");
        let err = ensure_protected(&roots, &overrides)
            .expect_err("an ordinary folder is no protected root");
        let text = format!("{err:#}");
        assert!(text.contains("is unsafe"), "unexpected refusal: {text}");
        assert!(
            std::fs::rename(&binaries, &aside).is_err(),
            "the binaries override was not held: renamed to {}",
            aside.display()
        );
    }

    /// The refusal comes from a check on the handle the guard holds: the
    /// refused folder is still held afterwards.
    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn the_check_runs_on_the_held_handle() {
        let data_local = scratch("held-check");
        let aside = scratch("held-check-aside");
        let roots = roots_for(
            Profile::Elevated,
            &Overrides::default(),
            &data_local.join("config"),
            &data_local,
        );
        let root = roots.root.clone().expect("an elevated profile has a root");
        std::fs::create_dir_all(&root).expect("create an ordinary folder at the root");
        let err = ensure_protected(&roots, &Overrides::default())
            .expect_err("an ordinary folder is no protected root");
        let text = format!("{err:#}");
        assert!(text.contains("is unsafe"), "unexpected refusal: {text}");
        assert!(
            std::fs::rename(&root, &aside).is_err(),
            "the refused root was not held: renamed to {}",
            aside.display()
        );
    }
}
