//! The daemon's profile: which set of folders and Credential Manager names a
//! daemon uses, decided by its token's integrity level.
//!
//! An ordinary daemon (below High integrity) keeps today's folders: config in
//! the roaming config dir, binaries, worktrees, `daemon-id`, tailnet and
//! summarizer under the machine-local data dir. An elevated daemon (at or above
//! High, the rule `daemon::integrity::IntegrityLevel::is_elevated` applies too)
//! keeps all of them under its own protected root, `<data_local>/elevated`, and
//! files its Credential Manager entries under a suffixed service name. An
//! override variable beats the profile for the folder it names.

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

#[cfg(windows)]
fn read_own_profile() -> anyhow::Result<Profile> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// A token handle closed when dropped.
    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: the handle was opened below and is closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    let mut token = HANDLE::default();
    // SAFETY: the current-process pseudo-handle needs no closing; `token`
    // receives a handle closed by `Token`.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
        .context("opening this process's token")?;
    let token = Token(token);
    let mut needed = 0u32;
    // SAFETY: a size query with no buffer; it fails with
    // ERROR_INSUFFICIENT_BUFFER and sets `needed`.
    let _ = unsafe { GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &raw mut needed) };
    if needed == 0 {
        return Err(anyhow!("this process's token reported no integrity label"));
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
    .context("reading this process's integrity label")?;
    // SAFETY: the call filled `buffer` with a TOKEN_MANDATORY_LABEL whose SID
    // points into the same buffer, which is alive and aligned.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    // SAFETY: `sid` is the valid label SID read above.
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    let Some(last) = count.checked_sub(1) else {
        return Err(anyhow!(
            "this process's integrity label has no sub-authority"
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
    let root = match profile {
        Profile::Ordinary => None,
        Profile::Elevated => Some(data_local.join("elevated")),
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
    let dirs = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve the app's folders"))?;
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
