#[cfg(not(windows))]
use anyhow::Result;
#[cfg(not(windows))]
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathTransition {
    Add,
    Remove,
    Restore,
}

#[cfg(not(windows))]
pub(crate) fn snapshot() -> Result<String> {
    anyhow::bail!("transactional User PATH operations are supported only on Windows")
}

#[cfg(not(windows))]
pub(crate) fn transition(_operation: PathTransition, _request: &Path) -> Result<String> {
    anyhow::bail!("transactional User PATH operations are supported only on Windows")
}

#[cfg(windows)]
mod windows {
    use super::PathTransition;
    use crate::fs_ops;
    use anyhow::{Context, Result};
    use serde::Deserialize;
    use std::ffi::c_void;
    use std::io::Read as _;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetLastError, SetLastError,
    };
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ,
        REG_VALUE_TYPE, RegCloseKey, RegDeleteValueW, RegOpenKeyTransactedW, RegQueryValueExW,
        RegSetValueExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    const REQUEST_MAX_BYTES: u64 = 1024 * 1024;
    const VALUE_MAX_BYTES: u32 = 1024 * 1024;
    const ENVIRONMENT_BROADCAST_TIMEOUT_MILLIS: u32 = 500;
    const ENVIRONMENT_KEY: &[u16] = &[
        b'E' as u16,
        b'n' as u16,
        b'v' as u16,
        b'i' as u16,
        b'r' as u16,
        b'o' as u16,
        b'n' as u16,
        b'm' as u16,
        b'e' as u16,
        b'n' as u16,
        b't' as u16,
        0,
    ];
    const PATH_VALUE: &[u16] = &[b'P' as u16, b'a' as u16, b't' as u16, b'h' as u16, 0];
    const ENVIRONMENT_NOTIFICATION: &[u16] = &[
        b'E' as u16,
        b'n' as u16,
        b'v' as u16,
        b'i' as u16,
        b'r' as u16,
        b'o' as u16,
        b'n' as u16,
        b'm' as u16,
        b'e' as u16,
        b'n' as u16,
        b't' as u16,
        0,
    ];

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum RegistrySnapshot {
        Absent,
        Present {
            value_type: REG_VALUE_TYPE,
            bytes: Vec<u8>,
        },
    }

    impl RegistrySnapshot {
        fn encode(&self) -> String {
            match self {
                Self::Absent => "absent".to_string(),
                Self::Present { value_type, bytes } => {
                    format!("v1:{value_type}:{}", hex::encode(bytes))
                }
            }
        }

        fn decode(encoded: &str) -> Result<Self> {
            if encoded == "absent" {
                return Ok(Self::Absent);
            }
            let mut fields = encoded.splitn(3, ':');
            if fields.next() != Some("v1") {
                anyhow::bail!("registry snapshot has an unsupported format");
            }
            let value_type: REG_VALUE_TYPE = fields
                .next()
                .context("registry snapshot is missing its value type")?
                .parse()
                .context("registry snapshot value type is not an integer")?;
            let bytes = hex::decode(
                fields
                    .next()
                    .context("registry snapshot is missing its raw value")?,
            )
            .context("registry snapshot raw value is not hexadecimal")?;
            validate_supported_value(value_type, &bytes)?;
            Ok(Self::Present { value_type, bytes })
        }
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EntryRequest {
        expected: String,
        entry: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RestoreRequest {
        expected: String,
        requested: String,
    }

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: the wrapper uniquely owns this key handle.
                RegCloseKey(self.0);
            }
        }
    }

    fn status_result(status: u32, action: &str) -> Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32)).context(action.to_string())
        }
    }

    fn open_environment(
        transaction: &fs_ops::WindowsTransaction,
        writable: bool,
    ) -> Result<RegistryKey> {
        let mut key = std::ptr::null_mut();
        let access = if writable {
            KEY_QUERY_VALUE | KEY_SET_VALUE
        } else {
            KEY_QUERY_VALUE
        };
        let status = unsafe {
            // SAFETY: all strings are NUL terminated, output points to one HKEY,
            // and transaction owns a live KTM transaction handle.
            RegOpenKeyTransactedW(
                HKEY_CURRENT_USER,
                ENVIRONMENT_KEY.as_ptr(),
                0,
                access,
                &mut key,
                transaction.handle(),
                std::ptr::null::<c_void>(),
            )
        };
        status_result(
            status,
            "opening HKCU\\Environment inside the registry transaction",
        )?;
        Ok(RegistryKey(key))
    }

    fn read_path(key: &RegistryKey) -> Result<RegistrySnapshot> {
        let mut value_type = 0;
        let mut byte_count = 0;
        let status = unsafe {
            // SAFETY: the key and NUL-terminated value name are valid; this
            // first query asks Windows only for type and exact byte count.
            RegQueryValueExW(
                key.0,
                PATH_VALUE.as_ptr(),
                std::ptr::null(),
                &mut value_type,
                std::ptr::null_mut(),
                &mut byte_count,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(RegistrySnapshot::Absent);
        }
        status_result(status, "reading the raw User PATH registry value size")?;
        if byte_count > VALUE_MAX_BYTES {
            anyhow::bail!("User PATH registry value exceeds the explicit transaction limit");
        }
        let mut bytes = vec![0_u8; byte_count as usize];
        let mut exact_count = byte_count;
        let status = unsafe {
            // SAFETY: bytes owns exact_count writable bytes (or a null pointer
            // for an empty value), and every other pointer is valid.
            RegQueryValueExW(
                key.0,
                PATH_VALUE.as_ptr(),
                std::ptr::null(),
                &mut value_type,
                if bytes.is_empty() {
                    std::ptr::null_mut()
                } else {
                    bytes.as_mut_ptr()
                },
                &mut exact_count,
            )
        };
        status_result(status, "reading the exact raw User PATH registry value")?;
        if exact_count != byte_count {
            anyhow::bail!("User PATH changed size inside one registry transaction");
        }
        validate_supported_value(value_type, &bytes)?;
        Ok(RegistrySnapshot::Present { value_type, bytes })
    }

    fn validate_supported_value(value_type: REG_VALUE_TYPE, bytes: &[u8]) -> Result<()> {
        if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
            anyhow::bail!(
                "User PATH has unsupported registry type {value_type}; it was left unchanged"
            );
        }
        if !bytes.len().is_multiple_of(2) {
            anyhow::bail!("User PATH contains an odd-length UTF-16 value; it was left unchanged");
        }
        let (pairs, remainder) = bytes.as_chunks::<2>();
        debug_assert!(remainder.is_empty());
        let units = pairs
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect::<Vec<_>>();
        if let Some(first_nul) = units.iter().position(|unit| *unit == 0)
            && units[first_nul..].iter().any(|unit| *unit != 0)
        {
            anyhow::bail!("User PATH contains an embedded NUL; it was left unchanged");
        }
        let content_end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        String::from_utf16(&units[..content_end])
            .context("User PATH contains invalid UTF-16; it was left unchanged")?;
        Ok(())
    }

    fn content_and_suffix(bytes: &[u8]) -> Result<(Vec<u16>, Vec<u16>)> {
        let (pairs, remainder) = bytes.as_chunks::<2>();
        debug_assert!(remainder.is_empty());
        let units = pairs
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect::<Vec<_>>();
        let content_end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        String::from_utf16(&units[..content_end]).context("User PATH is not valid UTF-16")?;
        Ok((units[..content_end].to_vec(), units[content_end..].to_vec()))
    }

    fn raw_bytes(content: &[u16], suffix: &[u16]) -> Vec<u8> {
        content
            .iter()
            .chain(suffix)
            .flat_map(|unit| unit.to_le_bytes())
            .collect()
    }

    fn ordinal_path_equal(left: &[u16], right: &[u16]) -> Result<bool> {
        let left_len: i32 = left.len().try_into().context("PATH entry is too long")?;
        let right_len: i32 = right.len().try_into().context("PATH entry is too long")?;
        let comparison = unsafe {
            // SAFETY: both slices remain alive for their explicitly supplied
            // lengths; CompareStringOrdinal does not require NUL termination.
            CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1)
        };
        if comparison == 0 {
            return Err(std::io::Error::last_os_error())
                .context("comparing Windows PATH entries with ordinal semantics");
        }
        Ok(comparison == CSTR_EQUAL)
    }

    fn transform_entry(
        current: &RegistrySnapshot,
        entry: &str,
        add: bool,
    ) -> Result<RegistrySnapshot> {
        if entry.is_empty() || entry.contains(';') || entry.contains('\0') {
            anyhow::bail!("installer PATH entry is empty or contains a separator/NUL");
        }
        let entry_units = entry.encode_utf16().collect::<Vec<_>>();
        let (value_type, content, suffix) = match current {
            RegistrySnapshot::Absent => {
                if !add {
                    return Ok(RegistrySnapshot::Absent);
                }
                (REG_EXPAND_SZ, Vec::new(), vec![0])
            }
            RegistrySnapshot::Present { value_type, bytes } => {
                let (content, suffix) = content_and_suffix(bytes)?;
                if suffix.is_empty() {
                    anyhow::bail!(
                        "User PATH string has no terminating NUL; entry transformation was refused without normalizing it"
                    );
                }
                (*value_type, content, suffix)
            }
        };
        let entries = content
            .split(|unit| *unit == b';' as u16)
            .collect::<Vec<_>>();
        let mut matching = Vec::with_capacity(entries.len());
        for value in &entries {
            matching.push(ordinal_path_equal(value, &entry_units)?);
        }
        if add && matching.iter().any(|matches| *matches) {
            return Ok(current.clone());
        }
        if !add && !matching.iter().any(|matches| *matches) {
            return Ok(current.clone());
        }

        let requested_content = if add {
            // A fully empty value is replaced by the exact entry, so the
            // installer never introduces a CWD-search segment. For any
            // nonempty raw value, append one delimiter and preserve every
            // existing empty segment (including a trailing one) verbatim.
            let mut requested = content;
            if !requested.is_empty() {
                requested.push(b';' as u16);
            }
            requested.extend_from_slice(&entry_units);
            requested
        } else {
            let mut requested = Vec::new();
            let mut first_kept = true;
            for (index, value) in entries.iter().enumerate() {
                if matching[index] {
                    continue;
                }
                if !first_kept {
                    requested.push(b';' as u16);
                }
                requested.extend_from_slice(value);
                first_kept = false;
            }
            if first_kept {
                return Ok(RegistrySnapshot::Absent);
            }
            requested
        };
        Ok(RegistrySnapshot::Present {
            value_type,
            bytes: raw_bytes(&requested_content, &suffix),
        })
    }

    fn write_path(key: &RegistryKey, requested: &RegistrySnapshot) -> Result<()> {
        match requested {
            RegistrySnapshot::Absent => {
                let status = unsafe {
                    // SAFETY: key and NUL-terminated value name are valid.
                    RegDeleteValueW(key.0, PATH_VALUE.as_ptr())
                };
                status_result(status, "deleting User PATH inside the registry transaction")
            }
            RegistrySnapshot::Present { value_type, bytes } => {
                let length: u32 = bytes.len().try_into().context("User PATH is too large")?;
                let status = unsafe {
                    // SAFETY: bytes remains valid for exactly length bytes and
                    // the key/value name are live for the call.
                    RegSetValueExW(
                        key.0,
                        PATH_VALUE.as_ptr(),
                        0,
                        *value_type,
                        if bytes.is_empty() {
                            std::ptr::null()
                        } else {
                            bytes.as_ptr()
                        },
                        length,
                    )
                };
                status_result(status, "writing User PATH inside the registry transaction")
            }
        }
    }

    fn read_request(path: &Path) -> Result<String> {
        let mut file = fs_ops::open_direct_regular(path)?;
        let before = fs_ops::token_for_file(&mut file)?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(REQUEST_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading registry request {}", path.display()))?;
        if bytes.len() as u64 > REQUEST_MAX_BYTES {
            anyhow::bail!("registry request exceeds the explicit size limit");
        }
        let after = fs_ops::token_for_file(&mut file)?;
        let final_path = fs_ops::token_for_path(path)?;
        if before != after || before != final_path {
            anyhow::bail!(
                "registry request changed while it was read; no registry transaction was attempted"
            );
        }
        String::from_utf8(bytes).context("registry request is not UTF-8")
    }

    fn broadcast_environment_change() -> std::result::Result<(), u32> {
        let mut message_result = 0_usize;
        unsafe {
            // SAFETY: SetLastError changes only this thread's Win32 error slot.
            // SendMessageTimeoutW is documented not to set it for every zero
            // return, so zero remains an explicit generic failure code.
            SetLastError(ERROR_SUCCESS);
        }
        let result = unsafe {
            // SAFETY: HWND_BROADCAST is the documented target for environment
            // updates and ENVIRONMENT_NOTIFICATION is a stable, NUL-terminated
            // UTF-16 string for the duration of this bounded synchronous call.
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                ENVIRONMENT_NOTIFICATION.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                ENVIRONMENT_BROADCAST_TIMEOUT_MILLIS,
                &mut message_result,
            )
        };
        if result == 0 {
            Err(unsafe {
                // SAFETY: this immediately follows the failed Win32 call on
                // the same thread and does not dereference any pointer.
                GetLastError()
            })
        } else {
            Ok(())
        }
    }

    fn transact(expected: RegistrySnapshot, requested: RegistrySnapshot) -> Result<String> {
        let transaction = fs_ops::WindowsTransaction::create("registry")?;
        let key = open_environment(&transaction, true)?;
        let observed = read_path(&key)?;
        if observed != expected {
            anyhow::bail!(
                "User PATH changed before the transactional CAS boundary; it was left unchanged"
            );
        }
        let changed = requested != expected;
        if changed {
            write_path(&key, &requested)?;
        }
        drop(key);
        transaction.commit("registry")?;
        let notification = if !changed {
            "unchanged".to_string()
        } else {
            match broadcast_environment_change() {
                Ok(()) => "broadcast-ok".to_string(),
                Err(error) => format!("broadcast-failed:{error}"),
            }
        };
        Ok(format!(
            "path-transition|{}|{notification}",
            requested.encode()
        ))
    }

    pub(crate) fn snapshot() -> Result<String> {
        let transaction = fs_ops::WindowsTransaction::create("registry")?;
        let key = open_environment(&transaction, false)?;
        read_path(&key).map(|snapshot| snapshot.encode())
    }

    pub(crate) fn transition(operation: PathTransition, request: &Path) -> Result<String> {
        let request = read_request(request)?;
        match operation {
            PathTransition::Add | PathTransition::Remove => {
                let request: EntryRequest =
                    serde_json::from_str(&request).context("parsing strict PATH request")?;
                let expected = RegistrySnapshot::decode(&request.expected)?;
                let requested =
                    transform_entry(&expected, &request.entry, operation == PathTransition::Add)?;
                transact(expected, requested)
            }
            PathTransition::Restore => {
                let request: RestoreRequest = serde_json::from_str(&request)
                    .context("parsing strict PATH restore request")?;
                let expected = RegistrySnapshot::decode(&request.expected)?;
                let requested = RegistrySnapshot::decode(&request.requested)?;
                transact(expected, requested)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{REG_EXPAND_SZ, RegistrySnapshot, transform_entry};

        fn snapshot(value: &str) -> RegistrySnapshot {
            let bytes = value
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            RegistrySnapshot::Present {
                value_type: REG_EXPAND_SZ,
                bytes,
            }
        }

        fn value(snapshot: RegistrySnapshot) -> String {
            let RegistrySnapshot::Present { bytes, .. } = snapshot else {
                panic!("expected present PATH")
            };
            let (pairs, remainder) = bytes.as_chunks::<2>();
            assert!(remainder.is_empty());
            let units = pairs
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .take_while(|unit| *unit != 0)
                .collect::<Vec<_>>();
            String::from_utf16(&units).unwrap()
        }

        #[test]
        fn remove_joins_only_kept_segments() {
            assert_eq!(
                value(transform_entry(&snapshot("A;B"), "A", false).unwrap()),
                "B"
            );
            assert_eq!(
                value(transform_entry(&snapshot("A;;B"), "A", false).unwrap()),
                ";B"
            );
            assert_eq!(
                value(transform_entry(&snapshot(";A;"), "A", false).unwrap()),
                ";"
            );
            assert_eq!(
                transform_entry(&snapshot("A"), "A", false).unwrap(),
                RegistrySnapshot::Absent
            );
        }

        #[test]
        fn entry_change_rejects_unterminated_raw_string() {
            let unterminated = RegistrySnapshot::Present {
                value_type: REG_EXPAND_SZ,
                bytes: "A".encode_utf16().flat_map(u16::to_le_bytes).collect(),
            };
            assert!(transform_entry(&unterminated, "B", true).is_err());
        }

        #[test]
        fn add_avoids_new_empty_segments_but_preserves_existing_ones() {
            assert_eq!(
                value(transform_entry(&snapshot(""), "Entry", true).unwrap()),
                "Entry"
            );
            assert_eq!(
                value(transform_entry(&snapshot("A"), "Entry", true).unwrap()),
                "A;Entry"
            );
            assert_eq!(
                value(transform_entry(&snapshot("A;"), "Entry", true).unwrap()),
                "A;;Entry"
            );
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::{snapshot, transition};
