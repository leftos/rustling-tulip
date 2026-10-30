//! Starts the daemon through WMI's `Win32_Process.Create`, so it runs outside
//! the caller's job object.
//!
//! The WMI provider host (`WmiPrvSE`) creates the process under the caller's
//! identity, and the new process belongs to none of the caller's jobs: it
//! outlives a kill-on-close job that forbids breakaway (a terminal's, an IDE
//! task runner's, a supervisor's). It inherits nothing from the caller, its
//! environment included, so the caller's whole environment is passed with the
//! call. That environment, secrets included, crosses into `WmiPrvSE`, which
//! runs as the same user.

use anyhow::{Context as _, anyhow, bail};
use std::ffi::{OsStr, OsString};
use std::iter::repeat_n;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use tracing::warn;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0_0_0, VT_ARRAY, VT_BSTR, VT_I4, VT_UNKNOWN, VariantClear,
};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_RETURN_WBEM_COMPLETE, WbemLocator,
};
use windows::core::{BSTR, HSTRING, IUnknown, Interface as _, PCWSTR};

const CREATE_NO_WINDOW: i32 = 0x0800_0000;
const DETACHED_PROCESS: i32 = 0x0000_0008;
const CREATE_UNICODE_ENVIRONMENT: i32 = 0x0000_0400;
const TEMPLATES_VAR: &str = "RUSTLING_TULIP_BIN_TEMPLATES";
const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;
const EQUALS: u16 = b'=' as u16;

/// Starts `bin` with `args` through `Win32_Process.Create`, in the caller's
/// current directory, with the caller's environment plus
/// `RUSTLING_TULIP_BIN_TEMPLATES=<template_dir>`, detached and windowless.
/// Returns the new process's id.
///
/// # Errors
///
/// Fails when COM cannot be initialised on this thread, any WMI call fails,
/// `Create` returns a non-zero `ReturnValue`, or a path, argument or
/// environment entry holds a NUL.
pub(crate) fn launch(bin: &Path, args: &[OsString], template_dir: &Path) -> anyhow::Result<u32> {
    let command_line = command_line(bin, args)?;
    let current_dir = std::env::current_dir().context("read the current directory")?;
    let current_dir = wide_without_nul(current_dir.as_os_str()).context("the current directory")?;
    let environment = environment(template_dir)?;
    let _apartment = ComApartment::enter()?;
    // A COM error can hold the thread's error-info object, which must not
    // outlive the apartment: keep only its text, before the guard drops.
    create_process(&command_line, &current_dir, &environment).map_err(|err| anyhow!("{err:#}"))
}

/// COM initialised on this thread for one launch, uninitialised on drop when
/// this call initialised it (also when it was already initialised the same
/// way, since each successful `CoInitializeEx` needs its `CoUninitialize`).
struct ComApartment {
    uninitialize: bool,
}

impl ComApartment {
    fn enter() -> anyhow::Result<Self> {
        // SAFETY: the reserved pointer must be null, which `None` passes.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            // Already initialised as a single-threaded apartment: usable, and
            // not ours to uninitialise.
            return Ok(Self {
                uninitialize: false,
            });
        }
        result
            .ok()
            .map_err(|err| anyhow!("initialise COM for the WMI launch: {err}"))?;
        Ok(Self { uninitialize: true })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: balances the successful CoInitializeEx in `enter`; every
            // COM object of the launch, the error info a failed call holds
            // included, was released before this guard drops (`launch` keeps
            // only an error's text).
            unsafe { CoUninitialize() };
        }
    }
}

/// The WMI half of [`launch`]; every COM object it holds is released when it
/// returns, and [`launch`] turns its error into plain text, before the
/// apartment guard drops.
fn create_process(
    command_line: &[u16],
    current_dir: &[u16],
    environment: &[Vec<u16>],
) -> anyhow::Result<u32> {
    // SAFETY: COM is initialised on this thread; WbemLocator is an in-process class.
    let locator: IWbemLocator =
        unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }
            .context("create the WMI locator")?;
    let empty = BSTR::new();
    // SAFETY: every BSTR argument is live for the call; no context object.
    let services = unsafe {
        locator.ConnectServer(
            &BSTR::from("ROOT\\CIMV2"),
            &empty,
            &empty,
            &empty,
            0,
            &empty,
            None,
        )
    }
    .context("connect to WMI's ROOT\\CIMV2 namespace")?;
    // SAFETY: `services` is a live proxy; no principal name and no auth info.
    unsafe {
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    }
    .context("set the WMI proxy's security")?;

    let startup = spawn_instance(&class(&services, "Win32_ProcessStartup")?)?;
    put(
        &startup,
        "CreateFlags",
        &Variant::i4(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT),
    )?;
    put(
        &startup,
        "EnvironmentVariables",
        &Variant::bstr_array(environment)?,
    )?;

    let process = class(&services, "Win32_Process")?;
    let mut in_signature = None;
    // SAFETY: the out pointer is a live Option; no out-signature is asked for.
    unsafe {
        process.GetMethod(
            &HSTRING::from("Create"),
            0,
            &raw mut in_signature,
            std::ptr::null_mut(),
        )
    }
    .context("read Win32_Process.Create's parameters")?;
    let in_signature = in_signature.context("Win32_Process.Create has no in-parameters")?;
    let params = spawn_instance(&in_signature)?;
    put(&params, "CommandLine", &Variant::bstr(command_line))?;
    put(&params, "CurrentDirectory", &Variant::bstr(current_dir))?;
    put(
        &params,
        "ProcessStartupInformation",
        &Variant::unknown(startup.cast()?),
    )?;

    let mut out = None;
    // SAFETY: the BSTRs and `params` are live for the call; the out pointer is a live Option.
    unsafe {
        services.ExecMethod(
            &BSTR::from("Win32_Process"),
            &BSTR::from("Create"),
            WBEM_FLAG_RETURN_WBEM_COMPLETE,
            None,
            &params,
            Some(&raw mut out),
            None,
        )
    }
    .context("call Win32_Process.Create")?;
    let out = out.context("Win32_Process.Create returned no out-parameters")?;
    let return_value = get_i4(&out, "ReturnValue")?;
    if return_value != 0 {
        bail!("Win32_Process.Create returned {return_value}");
    }
    let pid = get_i4(&out, "ProcessId")?;
    u32::try_from(pid).with_context(|| format!("Win32_Process.Create returned process id {pid}"))
}

fn class(services: &IWbemServices, name: &str) -> anyhow::Result<IWbemClassObject> {
    let mut object = None;
    // SAFETY: the BSTR is live for the call; the out pointer is a live Option.
    unsafe {
        services.GetObject(
            &BSTR::from(name),
            WBEM_FLAG_RETURN_WBEM_COMPLETE,
            None,
            Some(&raw mut object),
            None,
        )
    }
    .with_context(|| format!("read the WMI class {name}"))?;
    object.with_context(|| format!("WMI returned no {name} class"))
}

fn spawn_instance(class: &IWbemClassObject) -> anyhow::Result<IWbemClassObject> {
    // SAFETY: `class` is a live class object; flags must be 0.
    unsafe { class.SpawnInstance(0) }.context("create a WMI object instance")
}

fn put(object: &IWbemClassObject, name: &str, value: &Variant) -> anyhow::Result<()> {
    // SAFETY: the name and the VARIANT are live for the call; type 0 keeps the property's CIM type.
    unsafe { object.Put(&HSTRING::from(name), 0, &raw const value.0, 0) }
        .with_context(|| format!("set the WMI property {name}"))
}

fn get_i4(object: &IWbemClassObject, name: &str) -> anyhow::Result<i32> {
    let mut value = Variant(VARIANT::default());
    // SAFETY: the name is live for the call and the VARIANT is a live, empty one.
    unsafe { object.Get(&HSTRING::from(name), 0, &raw mut value.0, None, None) }
        .with_context(|| format!("read the WMI property {name}"))?;
    // SAFETY: Get filled the VARIANT; its type tag says which member is set.
    let fields = unsafe { &value.0.Anonymous.Anonymous };
    if fields.vt != VT_I4 {
        bail!(
            "the WMI property {name} is not a 32-bit integer (VARIANT type {})",
            fields.vt.0
        );
    }
    // SAFETY: the tag is VT_I4, so `lVal` is the member set.
    Ok(unsafe { fields.Anonymous.lVal })
}

/// An owned VARIANT, cleared (its string, array or interface released) on drop.
struct Variant(VARIANT);

impl Variant {
    fn tagged(vt: VARENUM, value: VARIANT_0_0_0) -> Self {
        let mut variant = VARIANT::default();
        // SAFETY: a zeroed VARIANT's first member is the tagged form written here.
        let fields = unsafe { &mut variant.Anonymous.Anonymous };
        fields.vt = vt;
        fields.Anonymous = value;
        Self(variant)
    }

    fn i4(value: i32) -> Self {
        Self::tagged(VT_I4, VARIANT_0_0_0 { lVal: value })
    }

    fn bstr(wide: &[u16]) -> Self {
        Self::tagged(
            VT_BSTR,
            VARIANT_0_0_0 {
                bstrVal: ManuallyDrop::new(BSTR::from_wide(wide)),
            },
        )
    }

    fn unknown(object: IUnknown) -> Self {
        Self::tagged(
            VT_UNKNOWN,
            VARIANT_0_0_0 {
                punkVal: ManuallyDrop::new(Some(object)),
            },
        )
    }

    /// A one-dimensional string array; the VARIANT owns the array and its copies.
    fn bstr_array(items: &[Vec<u16>]) -> anyhow::Result<Self> {
        let len = u32::try_from(items.len()).context("the environment has too many entries")?;
        // SAFETY: no pointer arguments; a null result is checked below.
        let array = unsafe { SafeArrayCreateVector(VT_BSTR, 0, len) };
        if array.is_null() {
            bail!("could not allocate the environment array");
        }
        let variant = Self::tagged(VT_ARRAY | VT_BSTR, VARIANT_0_0_0 { parray: array });
        for (index, item) in items.iter().enumerate() {
            let index = i32::try_from(index).context("the environment has too many entries")?;
            let entry = BSTR::from_wide(item);
            // SAFETY: `array` is live (owned by `variant`) and `index` is in its bounds; a
            // VT_BSTR element is passed as the BSTR itself, which the array copies.
            unsafe { SafeArrayPutElement(array, &raw const index, entry.as_ptr().cast()) }
                .context("fill the environment array")?;
        }
        Ok(variant)
    }
}

impl Drop for Variant {
    fn drop(&mut self) {
        // SAFETY: the VARIANT is a valid one this wrapper owns.
        if let Err(err) = unsafe { VariantClear(&raw mut self.0) } {
            warn!(%err, "could not release a VARIANT of the WMI launch");
        }
    }
}

/// The caller's environment as `NAME=value` entries, with
/// `RUSTLING_TULIP_BIN_TEMPLATES` set to `template_dir`.
fn environment(template_dir: &Path) -> anyhow::Result<Vec<Vec<u16>>> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| !name.to_string_lossy().eq_ignore_ascii_case(TEMPLATES_VAR))
        .collect();
    vars.push((TEMPLATES_VAR.into(), template_dir.as_os_str().to_owned()));
    vars.iter()
        .map(|(name, value)| {
            let context = || format!("environment variable {}", name.display());
            let mut entry = wide_without_nul(name).with_context(context)?;
            entry.push(EQUALS);
            entry.extend(wide_without_nul(value).with_context(context)?);
            Ok(entry)
        })
        .collect()
}

/// The program path in quotes, then each arg quoted as `CommandLineToArgvW`
/// and `std::env::args` parse it.
fn command_line(program: &Path, args: &[OsString]) -> anyhow::Result<Vec<u16>> {
    let program_wide = wide_without_nul(program.as_os_str()).context("the program path")?;
    if program_wide.contains(&QUOTE) {
        bail!("the program path {} holds a quote", program.display());
    }
    let mut line = vec![QUOTE];
    line.extend(program_wide);
    line.push(QUOTE);
    for (index, arg) in args.iter().enumerate() {
        line.push(SPACE);
        append_quoted(
            &mut line,
            &wide_without_nul(arg).with_context(|| format!("argument {index}"))?,
        );
    }
    Ok(line)
}

/// Appends `arg` to `line`, in quotes when it is empty or holds a space, tab or
/// quote. A quote is escaped with a backslash and the backslashes before it
/// doubled; backslashes before the closing quote are doubled too.
fn append_quoted(line: &mut Vec<u16>, arg: &[u16]) {
    let quote = arg.is_empty() || arg.iter().any(|&c| c == SPACE || c == TAB || c == QUOTE);
    if quote {
        line.push(QUOTE);
    }
    let mut backslashes = 0;
    for &c in arg {
        if c == BACKSLASH {
            backslashes += 1;
        } else {
            if c == QUOTE {
                line.extend(repeat_n(BACKSLASH, backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(c);
    }
    if quote {
        line.extend(repeat_n(BACKSLASH, backslashes));
        line.push(QUOTE);
    }
}

/// `s` as UTF-16, refused when it holds a NUL (which would end it early). The
/// error never quotes `s`, since a value can be a secret.
fn wide_without_nul(s: &OsStr) -> anyhow::Result<Vec<u16>> {
    let wide: Vec<u16> = s.encode_wide().collect();
    if wide.contains(&0) {
        bail!("holds a NUL character");
    }
    Ok(wide)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::launch;
    use crate::test_fakes::ScratchDir;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const HANG_GUARD: Duration = Duration::from_secs(120);
    /// Not representable in code page 1252, so an ANSI environment block mangles it.
    const PROBE: &str = "Łukasz-日本";

    /// The launched process reads a non-ANSI environment value exactly as the
    /// caller passed it. The value rides in `RUSTLING_TULIP_BIN_TEMPLATES`,
    /// which `launch` adds to the caller's environment, so the test leaves its
    /// own environment alone.
    #[test]
    #[ignore = "launches a real process through WMI; run by hand"]
    fn the_launched_process_reads_a_non_ansi_environment_value_intact() {
        let scratch = ScratchDir::new("wmi-unicode-env");
        let out = scratch.0.join("probe.txt");
        let staged = scratch.0.join("probe.tmp");
        let (out_text, staged_text) = (out.display(), staged.display());
        // PowerShell exits at once without a console, so the detached child is
        // cmd, whose /u writes an internal command's output as UTF-16LE.
        // No inner quotes: `launch` escapes a quote with a backslash, which cmd
        // does not read; the scratch path holds no space.
        let script = format!(
            "echo %RUSTLING_TULIP_BIN_TEMPLATES%>{staged_text}&& move /y {staged_text} {out_text} >nul"
        );
        let args: Vec<OsString> = ["/d", "/u", "/s", "/c", script.as_str()]
            .into_iter()
            .map(OsString::from)
            .collect();
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
        let cmd = PathBuf::from(system_root).join(r"System32\cmd.exe");

        launch(&cmd, &args, Path::new(PROBE)).expect("launch the probe through WMI");

        let written = wait_for_file(&out);
        let (pairs, rest) = written.as_chunks::<2>();
        assert!(rest.is_empty(), "odd-length UTF-16 output: {written:02x?}");
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        let read = String::from_utf16_lossy(&units);
        assert_eq!(
            read.strip_suffix("\r\n").unwrap_or(&read),
            PROBE,
            "the launched process read a different value (bytes {written:02x?})"
        );
    }

    /// The bytes of `path` once it exists, failing as hung after [`HANG_GUARD`].
    fn wait_for_file(path: &Path) -> Vec<u8> {
        let deadline = Instant::now() + HANG_GUARD;
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "hung: the launched process wrote no {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::read(path).expect("read the launched process's output")
    }
}
