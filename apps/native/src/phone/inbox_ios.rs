//! The phone's App Group container on iOS, where the share extension leaves
//! what other apps share ([`super::inbox`]).

use std::path::PathBuf;

use objc2_foundation::{NSFileManager, NSString};

use super::inbox::{GROUP_FOLDER, GROUP_ID};

/// The share inbox in the App Group container; `None` while the app is not
/// signed into the group, as the system then names no container.
pub(super) fn group_container() -> Option<PathBuf> {
    let container = NSFileManager::defaultManager()
        .containerURLForSecurityApplicationGroupIdentifier(&NSString::from_str(GROUP_ID))?;
    let path = container.path()?;
    Some(PathBuf::from(path.to_string()).join(GROUP_FOLDER))
}
