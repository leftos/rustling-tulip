//! How a toast press reaches the client: the app's own app id, the
//! Start-menu shortcut and the HKCU COM registration that carry it, the COM
//! activator Windows calls on every press (from the popup or from Action
//! Center), the toast XML, and the launch arguments a press starts a closed
//! client with.
//!
//! Only the installed copy (the installer's `uninstall.exe` beside it)
//! registers; a build-tree exe, or one whose registration fails, raises its
//! toasts under the PowerShell app id with in-process handlers, which a
//! press from Action Center never reaches.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::notify::PairingActionSink;
use crate::pairing::{PairingAction, PairingToast};

/// The app id the client's toasts carry once registered.
pub const APP_ID: &str = "leftos.RustlingTulip";

/// The toast activator's class id, `{354BE9DC-6D82-4269-8883-D13CE3E38648}`.
/// It is written into the Start-menu shortcut and the HKCU class key, so it
/// never changes.
pub const ACTIVATOR_CLSID: u128 = 0x354B_E9DC_6D82_4269_8883_D13C_E3E3_8648;

/// The app id toasts fall back to while the client has no registration.
pub const POWERSHELL_APP_ID: &str =
    "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

/// The argument the activator's registered command starts the client with.
pub const TOAST_ACTIVATED_FLAG: &str = "-ToastActivated";

/// The argument COM appends to every command it starts a local server with.
const EMBEDDING_FLAG: &str = "-Embedding";

/// The Start-menu shortcut's file name: the one the installer writes, so
/// there is one shortcut.
pub const SHORTCUT_NAME: &str = "rustling-tulip.lnk";

/// What the client was started for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The session to focus once it arrives.
    pub session: Option<String>,
    /// Whether a toast press started the client through its activator.
    pub toast_activated: bool,
}

/// Reads the launch arguments after the program name: `-ToastActivated`
/// marks a start by a toast press, COM's `-Embedding` is ignored, and the
/// first other argument is a session id to focus.
#[must_use]
pub fn parse_launch_args(args: impl IntoIterator<Item = String>) -> Launch {
    let mut launch = Launch {
        session: None,
        toast_activated: false,
    };
    for arg in args {
        let embedding =
            arg.eq_ignore_ascii_case(EMBEDDING_FLAG) || arg.eq_ignore_ascii_case("/Embedding");
        if arg.eq_ignore_ascii_case(TOAST_ACTIVATED_FLAG) {
            launch.toast_activated = true;
        } else if !embedding && launch.session.is_none() {
            launch.session = Some(arg);
        }
    }
    launch
}

/// The activator's class id in registry form, `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
#[must_use]
pub fn clsid_braced() -> String {
    let id = ACTIVATOR_CLSID;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        id >> 96,
        (id >> 80) & 0xFFFF,
        (id >> 64) & 0xFFFF,
        (id >> 48) & 0xFFFF,
        id & 0xFFFF_FFFF_FFFF
    )
}

/// The HKCU key, under `Software\Classes`, that holds the activator's class.
#[must_use]
pub fn activator_class_key() -> String {
    format!("Software\\Classes\\CLSID\\{}", clsid_braced())
}

/// One registry string value to write: `name` `None` is the key's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryValue {
    pub key: String,
    pub name: Option<&'static str>,
    pub data: String,
}

/// The HKCU values that point the activator's class at `exe`: the command
/// COM starts a closed client with, and the executable it names.
#[must_use]
pub fn activator_registry_entries(exe: &Path) -> Vec<RegistryValue> {
    let key = format!("{}\\LocalServer32", activator_class_key());
    let exe = exe.display().to_string();
    vec![
        RegistryValue {
            key: key.clone(),
            name: None,
            data: format!("\"{exe}\" {TOAST_ACTIVATED_FLAG}"),
        },
        RegistryValue {
            key,
            name: Some("ServerExecutable"),
            data: exe,
        },
    ]
}

/// The uninstaller the installer writes beside the client it installs.
const UNINSTALLER: &str = "uninstall.exe";

/// Whether `exe` is the installed copy: the installer's uninstaller sits
/// beside it, as `exists` reports. A build-tree exe never has one.
#[must_use]
pub fn installed_copy(exe: &Path, exists: impl Fn(&Path) -> bool) -> bool {
    exe.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .is_some_and(|dir| exists(&dir.join(UNINSTALLER)))
}

/// The Start-menu shortcut inside the user's Programs folder `programs`.
#[must_use]
pub fn shortcut_path(programs: &Path) -> PathBuf {
    programs.join(SHORTCUT_NAME)
}

/// How a toast button activates the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivationKind {
    /// Brings the app forward.
    Foreground,
    /// Reaches the app without bringing it forward.
    Background,
}

impl ActivationKind {
    const fn attribute(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
        }
    }
}

/// A toast button: its label, the argument the activator receives, and how
/// it activates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToastButton {
    pub(crate) content: &'static str,
    pub(crate) arguments: String,
    pub(crate) kind: ActivationKind,
}

/// A toast's XML: `title` and `body` as its two lines, then `buttons`.
/// `activation_types` writes each button's activation type; without it every
/// button takes Windows' default, a foreground activation.
#[must_use]
pub(crate) fn toast_xml(
    title: &str,
    body: &str,
    buttons: &[ToastButton],
    activation_types: bool,
) -> String {
    let mut xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>",
        escape(title),
        escape(body)
    );
    if !buttons.is_empty() {
        xml.push_str("<actions>");
        for button in buttons {
            xml.push_str("<action content=\"");
            xml.push_str(&escape(button.content));
            xml.push_str("\" arguments=\"");
            xml.push_str(&escape(&button.arguments));
            xml.push('"');
            if activation_types {
                xml.push_str(" activationType=\"");
                xml.push_str(button.kind.attribute());
                xml.push('"');
            }
            xml.push_str("/>");
        }
        xml.push_str("</actions>");
    }
    xml.push_str("</toast>");
    xml
}

/// The pairing toast's XML: Approve… brings the app forward to open the
/// approval dialog, and Deny declines without raising a window.
#[must_use]
pub(crate) fn pairing_toast_xml(toast: &PairingToast, activation_types: bool) -> String {
    let buttons = [
        ToastButton {
            content: "Approve…",
            arguments: PairingAction::approve_arg(&toast.request_id),
            kind: ActivationKind::Foreground,
        },
        ToastButton {
            content: "Deny",
            arguments: PairingAction::deny_arg(&toast.request_id),
            kind: ActivationKind::Background,
        },
    ];
    toast_xml(&toast.title, &toast.body, &buttons, activation_types)
}

/// `text` with XML's five special characters escaped.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Whether this launch registered the activator: set once at startup.
static REGISTERED: OnceLock<bool> = OnceLock::new();

/// Whether the client's toasts carry its own app id and activate through
/// its activator.
#[must_use]
pub(crate) fn registered() -> bool {
    REGISTERED.get().copied().unwrap_or(false)
}

/// The app id toasts are raised under.
#[must_use]
pub(crate) fn app_id() -> &'static str {
    if registered() {
        APP_ID
    } else {
        POWERSHELL_APP_ID
    }
}

/// Where the activator hands what was pressed: the window's sink once it is
/// attached, and until then the presses that arrived before it.
#[derive(Default)]
struct Delivery {
    sink: Option<PairingActionSink>,
    early: Vec<PairingAction>,
}

static DELIVERY: Mutex<Delivery> = Mutex::new(Delivery {
    sink: None,
    early: Vec::new(),
});

fn delivery() -> MutexGuard<'static, Delivery> {
    // A panic while the lock was held leaves the delivery state whole: each
    // write is a single push or assignment.
    DELIVERY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hands the activator's presses to `sink` from now on, first the ones that
/// arrived before the window did.
pub(crate) fn attach(sink: &PairingActionSink) {
    let early = {
        let mut delivery = delivery();
        delivery.sink = Some(Arc::clone(sink));
        std::mem::take(&mut delivery.early)
    };
    for action in early {
        sink(action);
    }
}

/// A toast was pressed with `arg`: hands its action to the window, or holds
/// it until the window attaches. An argument no action knows is logged and
/// dropped.
fn deliver(arg: Option<&str>) {
    let Some(action) = PairingAction::from_activation(arg) else {
        tracing::warn!(?arg, "a toast was pressed with an argument no action knows");
        return;
    };
    tracing::info!(?action, "a toast was pressed");
    let sink = {
        let mut delivery = delivery();
        let Some(sink) = delivery.sink.clone() else {
            delivery.early.push(action);
            return;
        };
        sink
    };
    sink(action);
}

/// Registers the toast activator for this launch: the HKCU class pointing
/// at this exe, the Start-menu shortcut carrying the app id and the class,
/// and the class object COM hands presses to, then gives the process the
/// app id so its windows group with the Start-menu entry. Only the
/// installed copy registers; any other exe (a build tree's) registers
/// nothing, and a failure is logged. Either way toasts then stay under the
/// PowerShell app id.
pub fn register() {
    let registered = match std::env::current_exe() {
        Ok(exe) if installed_copy(&exe, Path::exists) => register_activator(exe),
        Ok(_) => {
            tracing::info!("not the installed copy: toasts stay under the PowerShell app id");
            false
        }
        Err(err) => {
            tracing::warn!(
                "toasts stay under the PowerShell app id: this exe's path is unknown: {err}"
            );
            false
        }
    };
    if REGISTERED.set(registered).is_err() {
        tracing::warn!("the toast activator was registered twice; the first result stands");
    }
}

#[cfg(windows)]
fn register_activator(exe: PathBuf) -> bool {
    let (done, result) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("toast-activator".to_owned())
        .spawn(move || com::serve(&done, &exe));
    if let Err(err) = spawned {
        tracing::warn!(
            "toasts stay under the PowerShell app id: the activator thread did not start: {err}"
        );
        return false;
    }
    match result.recv() {
        Ok(Ok(())) => {
            tracing::info!(app_id = APP_ID, "toast activator registered");
            com::set_process_app_id();
            true
        }
        Ok(Err(err)) => {
            tracing::warn!(
                "toasts stay under the PowerShell app id: registering the toast activator failed: {err:#}"
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                "toasts stay under the PowerShell app id: the activator thread ended without a result: {err}"
            );
            false
        }
    }
}

#[cfg(not(windows))]
fn register_activator(_exe: PathBuf) -> bool {
    false
}

#[cfg(windows)]
#[expect(
    clippy::inline_always,
    clippy::ref_as_ptr,
    reason = "the windows crate's #[implement] expands to code with these"
)]
mod com {
    //! The COM side: the class factory and activator, the HKCU values, the
    //! shortcut, and the thread that keeps the class registered.

    use std::path::Path;
    use std::sync::mpsc::Sender;

    use anyhow::Context as _;
    use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, ERROR_SUCCESS};
    use windows::Win32::Storage::EnhancedStorage::{
        PKEY_AppUserModel_ID, PKEY_AppUserModel_ToastActivatorCLSID,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        InitPropVariantFromCLSID, PROPVARIANT, PVCHF_DEFAULT, PropVariantChangeType,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance,
        CoInitializeEx, CoRegisterClassObject, CoTaskMemFree, IClassFactory, IClassFactory_Impl,
        IPersistFile, REGCLS_MULTIPLEUSE,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegSetValueExW,
    };
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Notifications::{
        INotificationActivationCallback, INotificationActivationCallback_Impl,
        NOTIFICATION_USER_INPUT_DATA,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{
        FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
        SetCurrentProcessExplicitAppUserModelID, ShellLink,
    };
    use windows::core::{BOOL, GUID, HSTRING, IUnknown, Interface as _, PCWSTR, Ref, implement};

    use super::{APP_ID, RegistryValue, activator_registry_entries, deliver, shortcut_path};

    const CLSID: GUID = GUID::from_u128(super::ACTIVATOR_CLSID);

    /// Joins the multithreaded apartment, registers, reports the result on
    /// `done`, and on success parks for the rest of the process so the
    /// apartment and the class registration stay alive.
    pub(super) fn serve(done: &Sender<anyhow::Result<()>>, exe: &Path) {
        // SAFETY: called once, first thing on this thread.
        let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("joining COM's multithreaded apartment");
        let result = joined.and_then(|()| setup(exe));
        let serving = result.is_ok();
        if done.send(result).is_err() {
            tracing::warn!("the toast activator's result had no one to read it");
        }
        if serving {
            loop {
                std::thread::park();
            }
        }
    }

    /// Gives this process the app id, so a window opened from any shortcut
    /// or a toast groups with the Start-menu entry on the taskbar. A failure
    /// is logged; toasts work without it.
    pub(super) fn set_process_app_id() {
        // SAFETY: a plain call with a string that outlives it, made before
        // the first window opens.
        if let Err(err) = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)) }
        {
            tracing::warn!("the process could not take the app id {APP_ID}: {err}");
        }
    }

    fn setup(exe: &Path) -> anyhow::Result<()> {
        write_registry(HKEY_CURRENT_USER, &activator_registry_entries(exe))?;
        let shortcut = shortcut_path(&programs_dir()?);
        write_shortcut(&shortcut, exe)
            .with_context(|| format!("writing the shortcut {}", shortcut.display()))?;
        let factory: IClassFactory = ActivatorFactory.into();
        // SAFETY: the factory is a live COM object; COM keeps its own
        // reference for as long as the class stays registered.
        unsafe { CoRegisterClassObject(&CLSID, &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE) }
            .context("registering the toast activator's class object")?;
        Ok(())
    }

    /// The user's Start-menu Programs folder.
    fn programs_dir() -> anyhow::Result<std::path::PathBuf> {
        // SAFETY: the returned string is read once, then freed with
        // CoTaskMemFree as the API requires.
        unsafe {
            let raw = SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None)
                .context("finding the Start menu's Programs folder")?;
            let path = raw.to_string();
            CoTaskMemFree(Some(raw.0.cast_const().cast()));
            Ok(std::path::PathBuf::from(path.context(
                "reading the Start menu's Programs folder path",
            )?))
        }
    }

    /// Writes each of `values` as a string under `root`.
    pub(super) fn write_registry(root: HKEY, values: &[RegistryValue]) -> anyhow::Result<()> {
        for value in values {
            let mut key = HKEY::default();
            // SAFETY: every pointer is to a live local; the key opened here
            // is closed below.
            let created = unsafe {
                RegCreateKeyExW(
                    root,
                    &HSTRING::from(value.key.as_str()),
                    None,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE,
                    None,
                    &raw mut key,
                    None,
                )
            };
            if created != ERROR_SUCCESS {
                anyhow::bail!(
                    "creating HKCU\\{}: {}",
                    value.key,
                    created.to_hresult().message()
                );
            }
            let name = value.name.map(HSTRING::from);
            let bytes: Vec<u8> = value
                .data
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            // SAFETY: `key` is open, and `name` and `bytes` outlive the call.
            let set = unsafe {
                RegSetValueExW(
                    key,
                    name.as_ref()
                        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr())),
                    None,
                    REG_SZ,
                    Some(&bytes),
                )
            };
            // SAFETY: `key` was opened above and is not used again.
            let closed = unsafe { RegCloseKey(key) };
            if set != ERROR_SUCCESS {
                anyhow::bail!(
                    "writing HKCU\\{} {:?}: {}",
                    value.key,
                    value.name,
                    set.to_hresult().message()
                );
            }
            if closed != ERROR_SUCCESS {
                tracing::warn!(
                    "closing HKCU\\{}: {}",
                    value.key,
                    closed.to_hresult().message()
                );
            }
        }
        Ok(())
    }

    /// Writes the shortcut at `path` to `exe`, carrying the app id and the
    /// activator's class id.
    fn write_shortcut(path: &Path, exe: &Path) -> anyhow::Result<()> {
        // SAFETY: plain COM calls on objects created here, on a thread that
        // joined the multithreaded apartment.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(exe))?;
            if let Some(dir) = exe.parent() {
                link.SetWorkingDirectory(&HSTRING::from(dir))?;
            }
            let store: IPropertyStore = link.cast()?;
            let text = PROPVARIANT::from(APP_ID);
            let mut app_id = PROPVARIANT::default();
            PropVariantChangeType(&raw mut app_id, &raw const text, PVCHF_DEFAULT, VT_LPWSTR)?;
            store.SetValue(&PKEY_AppUserModel_ID, &raw const app_id)?;
            let clsid = InitPropVariantFromCLSID(&CLSID)?;
            store.SetValue(&PKEY_AppUserModel_ToastActivatorCLSID, &raw const clsid)?;
            store.Commit()?;
            let file: IPersistFile = link.cast()?;
            file.Save(&HSTRING::from(path), true)?;
        }
        Ok(())
    }

    /// Hands out the activator COM asks for on each press.
    #[implement(IClassFactory)]
    struct ActivatorFactory;

    impl IClassFactory_Impl for ActivatorFactory_Impl {
        fn CreateInstance(
            &self,
            outer: Ref<'_, IUnknown>,
            riid: *const GUID,
            object: *mut *mut core::ffi::c_void,
        ) -> windows::core::Result<()> {
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let activator: INotificationActivationCallback = Activator.into();
            // SAFETY: COM passes a valid interface id and out pointer.
            unsafe { activator.query(riid, object) }.ok()
        }

        fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Receives a toast press and hands its argument to the window.
    #[implement(INotificationActivationCallback)]
    struct Activator;

    impl INotificationActivationCallback_Impl for Activator_Impl {
        fn Activate(
            &self,
            _app_id: &PCWSTR,
            invoked_args: &PCWSTR,
            _data: *const NOTIFICATION_USER_INPUT_DATA,
            _count: u32,
        ) -> windows::core::Result<()> {
            if invoked_args.is_null() {
                deliver(None);
                return Ok(());
            }
            // SAFETY: COM passes a null-terminated string that lives for
            // the call.
            match unsafe { invoked_args.to_string() } {
                Ok(arg) => deliver(Some(&arg)),
                Err(err) => {
                    tracing::warn!(
                        "a toast was pressed with an argument that is not UTF-16: {err}"
                    );
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        ActivationKind, ToastButton, activator_registry_entries, clsid_braced, installed_copy,
        pairing_toast_xml, parse_launch_args, shortcut_path, toast_xml,
    };
    use crate::pairing::PairingToast;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn toast(body: &str) -> PairingToast {
        PairingToast {
            request_id: "r1".to_owned(),
            title: "A device asks for remote access".to_owned(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn pairing_toast_xml_has_two_buttons_with_their_activation_types() {
        let xml = pairing_toast_xml(&toast("phone"), true);
        assert_eq!(xml.matches("<action ").count(), 2, "{xml}");
        assert!(
            xml.contains("<action content=\"Approve…\" arguments=\"approve:r1\" activationType=\"foreground\"/>"),
            "{xml}"
        );
        assert!(
            xml.contains(
                "<action content=\"Deny\" arguments=\"deny:r1\" activationType=\"background\"/>"
            ),
            "{xml}"
        );
        assert!(
            xml.starts_with("<toast><visual><binding template=\"ToastGeneric\"><text>A device asks for remote access</text><text>phone</text>"),
            "{xml}"
        );
        let fallback = pairing_toast_xml(&toast("phone"), false);
        assert!(
            !fallback.contains("activationType"),
            "the fallback keeps Windows' default: {fallback}"
        );
        assert_eq!(fallback.matches("<action ").count(), 2, "{fallback}");
    }

    #[test]
    fn toast_xml_escapes_the_device_name() {
        let xml = pairing_toast_xml(&toast("<b>&'\"phone"), true);
        assert!(
            xml.contains("<text>&lt;b&gt;&amp;&apos;&quot;phone</text>"),
            "{xml}"
        );
        let button = ToastButton {
            content: "Deny",
            arguments: "deny:a\"b".to_owned(),
            kind: ActivationKind::Background,
        };
        let xml = toast_xml("t", "b", &[button], true);
        assert!(xml.contains("arguments=\"deny:a&quot;b\""), "{xml}");
    }

    #[test]
    fn toast_xml_without_buttons_has_no_actions() {
        assert_eq!(
            toast_xml("Needs you", "fix login", &[], true),
            "<toast><visual><binding template=\"ToastGeneric\"><text>Needs you</text><text>fix login</text></binding></visual></toast>"
        );
    }

    #[test]
    fn launch_args_recognise_toast_activated_and_a_session_id() {
        let none = parse_launch_args(args(&[]));
        assert_eq!(none.session, None);
        assert!(!none.toast_activated);

        let session = parse_launch_args(args(&["sess-1"]));
        assert_eq!(session.session.as_deref(), Some("sess-1"));
        assert!(!session.toast_activated);

        let toast = parse_launch_args(args(&["-ToastActivated", "-Embedding"]));
        assert_eq!(toast.session, None, "neither flag is a session id");
        assert!(toast.toast_activated);

        let lower = parse_launch_args(args(&["-toastactivated"]));
        assert!(lower.toast_activated, "Windows flags are case-insensitive");
        assert_eq!(lower.session, None);

        let embedding = parse_launch_args(args(&["/Embedding"]));
        assert_eq!(embedding.session, None);
        assert!(!embedding.toast_activated);
    }

    #[test]
    fn activator_registry_entries_name_the_exe_and_clsid() {
        assert_eq!(clsid_braced(), "{354BE9DC-6D82-4269-8883-D13CE3E38648}");
        let exe = Path::new(
            r"C:\Users\me\AppData\Local\Programs\rustling-tulip\rustling-tulip-native.exe",
        );
        let entries = activator_registry_entries(exe);
        assert_eq!(entries.len(), 2, "{entries:?}");
        let key = r"Software\Classes\CLSID\{354BE9DC-6D82-4269-8883-D13CE3E38648}\LocalServer32";
        assert!(entries.iter().all(|entry| entry.key == key), "{entries:?}");
        assert_eq!(entries[0].name, None, "the command is the key's default");
        assert_eq!(
            entries[0].data,
            r#""C:\Users\me\AppData\Local\Programs\rustling-tulip\rustling-tulip-native.exe" -ToastActivated"#
        );
        assert_eq!(entries[1].name, Some("ServerExecutable"));
        assert_eq!(
            entries[1].data,
            r"C:\Users\me\AppData\Local\Programs\rustling-tulip\rustling-tulip-native.exe"
        );
    }

    #[test]
    fn registers_only_beside_the_installers_uninstaller() {
        let installed = Path::new(r"C:\Users\me\AppData\Local\Programs\rustling-tulip");
        let exe = installed.join("rustling-tulip-native.exe");
        let beside = |path: &Path| path == installed.join("uninstall.exe");
        assert!(installed_copy(&exe, beside), "the installed copy");
        let build_tree =
            Path::new(r"X:\dev\rustling-tulip\target\test-release\rustling-tulip-native.exe");
        assert!(
            !installed_copy(build_tree, beside),
            "a build-tree exe has no uninstaller beside it"
        );
        assert!(
            !installed_copy(&exe, |_: &Path| false),
            "the uninstaller removed or never written"
        );
        assert!(
            !installed_copy(Path::new("rustling-tulip-native.exe"), |_: &Path| true),
            "a bare file name has no folder to look in"
        );
    }

    #[test]
    fn activator_shortcut_is_the_installer_one() {
        assert_eq!(
            shortcut_path(Path::new(r"C:\Start\Programs")),
            Path::new(r"C:\Start\Programs\rustling-tulip.lnk")
        );
    }
}
